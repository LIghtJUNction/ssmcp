//! Small append-only activity summaries; never owns or schedules a job.
use serde_json::Value;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone)]
pub(super) struct Activity {
    pub id: String,
    pub description: String,
    pub cwd: Option<String>,
    pub started: Instant,
}

#[derive(Default)]
pub(super) struct Activities {
    active: BTreeMap<String, Activity>,
    frame: usize,
}

impl Activities {
    pub fn start(
        &mut self,
        key: String,
        id: String,
        description: String,
        cwd: Option<String>,
        now: Instant,
    ) {
        self.active.entry(key).or_insert(Activity {
            id,
            description,
            cwd,
            started: now,
        });
    }

    pub fn finish(&mut self, key: &str) -> Option<Activity> {
        self.active.remove(key)
    }

    pub fn len(&self) -> usize {
        self.active.len()
    }

    pub fn snapshot(&self) -> Vec<Activity> {
        let mut activities = self.active.values().cloned().collect::<Vec<_>>();
        activities.sort_by_key(|activity| activity.started);
        activities
    }

    pub fn summary(&mut self, now: Instant) -> Option<String> {
        if self.active.is_empty() {
            return None;
        }
        // A changing activity mark is not a measurement of completion.
        let spinner = ["·", "◦", "○", "◦"][self.frame % 4];
        self.frame = self.frame.wrapping_add(1);
        let mut items = self
            .active
            .values()
            .take(3)
            .map(|activity| {
                let duration = elapsed(now.saturating_duration_since(activity.started));
                if self.active.len() > 1 {
                    format!("{} {duration}", short_id(&activity.id))
                } else {
                    format!(
                        "{} {} · {duration}",
                        short_id(&activity.id),
                        single_line(&activity.description, 36)
                    )
                }
            })
            .collect::<Vec<_>>();
        if self.active.len() > items.len() {
            items.push(format!("+{} more", self.active.len() - items.len()));
        }
        Some(format!(
            "{spinner} {} running · {}",
            self.active.len(),
            items.join("  |  ")
        ))
    }
}

pub(super) fn key(kind: &str, label: &str, id: &str) -> String {
    format!("{kind}\0{label}\0{id}")
}

pub(super) fn short_id(id: &str) -> String {
    if let Some((nonce, number)) = id.rsplit_once('-')
        && nonce.len() == 32
        && nonce.bytes().all(|c| c.is_ascii_hexdigit())
        && !number.is_empty()
        && number.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return format!("#{number}");
    }
    if id.chars().count() <= 18 {
        id.into()
    } else {
        let end = id
            .chars()
            .rev()
            .take(12)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>();
        format!("…{end}")
    }
}

pub(super) fn elapsed(duration: Duration) -> String {
    if duration.as_secs() < 60 {
        format!("{:.1}s", duration.as_secs_f64())
    } else {
        format!(
            "{}m {:02}s",
            duration.as_secs() / 60,
            duration.as_secs() % 60
        )
    }
}

pub(super) fn single_line(text: &str, limit: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    super::super::panel::fit(&line, limit)
}

/// Only an explicit native progress and positive total can produce a percentage.
pub(super) fn progress(params: &Value) -> String {
    let amount = params["progress"]
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.0);
    let total = params["total"]
        .as_f64()
        .filter(|value| value.is_finite() && *value > 0.0);
    if let Some((amount, total)) = amount.zip(total).filter(|(amount, total)| amount <= total) {
        let filled = (amount / total * 10.0).floor() as usize;
        return format!(
            "{}{} {:.0}% ({amount}/{total})",
            "━".repeat(filled),
            "─".repeat(10 - filled),
            amount / total * 100.0
        );
    }
    match amount {
        Some(amount) => format!("progress {amount} · total unknown"),
        None => "in progress · total unknown".into(),
    }
}

/// Observed callers are display groups, never authenticated recipient identities.
#[derive(Default)]
pub(crate) struct CallerTracker {
    runs: std::collections::VecDeque<(String, String)>,
    stale: std::collections::BTreeSet<String>,
}

#[derive(Debug)]
pub(crate) struct CallerGroup {
    pub key: String,
    pub name: String,
    pub model: String,
    pub source: String,
    pub run_id: String,
    pub tool: String,
    pub calls: usize,
    pub running: usize,
    pub unknown: bool,
    pub status: String,
    representative: (u8, std::cmp::Reverse<u64>),
}

impl CallerTracker {
    /// Retire every observed start even if no frame is drawn before reconnect.
    /// This affects display confidence only; audit entries and recipients are untouched.
    pub(crate) fn disconnect(&mut self) {
        self.stale
            .extend(self.runs.iter().map(|(id, _)| id.clone()));
    }

    pub(crate) fn observe(&mut self, event: &Value, data: &Value) {
        let run = field(event, "run_id");
        if field(event, "phase") != "started"
            || run.is_empty()
            || run.len() > 256
            || !data["connected"].as_bool().unwrap_or(false)
            || self.runs.iter().any(|(id, _)| id == run)
        {
            return;
        }
        let room = field(data, "room_id");
        if room.len() > 256 {
            return;
        }
        self.runs.push_back((run.into(), room.into()));
        while self.runs.len() > 256 {
            if let Some((id, _)) = self.runs.pop_front() {
                self.stale.remove(&id);
            }
        }
    }

    pub(crate) fn groups(&mut self, observed: &[Value], data: &Value) -> Vec<CallerGroup> {
        let preview = data["preview"].as_bool().unwrap_or(false);
        let connected = preview || data["connected"].as_bool().unwrap_or(false);
        if !connected {
            self.disconnect();
        }
        let mut groups: Vec<CallerGroup> = Vec::new();
        let mut indexes = BTreeMap::<(String, String, String), usize>::new();
        for entry in observed.iter().take(128) {
            let run = field(entry, "run_id");
            let name = field(entry, "name");
            let model = field(entry, "model");
            let source = field(entry, "source");
            let known =
                !name.is_empty() && name != "Unknown controller" && source != "metadata missing";
            let key = if known {
                (source.into(), name.into(), model.into())
            } else {
                (String::new(), run.into(), String::new())
            };
            // This is a stable display key, not an authenticated recipient ID.
            // Encoding the tuple avoids collisions between untrusted labels.
            let display_key = serde_json::json!(key).to_string();
            let index = *indexes.entry(key).or_insert_with(|| {
                groups.push(CallerGroup {
                    key: display_key,
                    name: if known {
                        super::super::panel::safe_text(name)
                    } else {
                        format!("Unknown #{}", run.chars().take(6).collect::<String>())
                    },
                    model: super::super::panel::safe_text(model),
                    source: source.into(),
                    run_id: run.into(),
                    tool: super::super::panel::safe_text(field(entry, "tool")),
                    calls: 0,
                    running: 0,
                    unknown: false,
                    status: if connected {
                        super::super::panel::safe_text(field(entry, "status"))
                    } else {
                        "offline".into()
                    },
                    representative: (0, std::cmp::Reverse(u64::MAX)),
                });
                groups.len() - 1
            });
            let current = connected
                && (preview
                    || (!self.stale.contains(run)
                        && self
                            .runs
                            .iter()
                            .any(|(id, room)| id == run && room == field(data, "room_id"))));
            let group = &mut groups[index];
            group.calls += 1;
            let active = entry["active"].as_bool().unwrap_or(false);
            // Audit display sorts unfinished calls first. Its original order
            // still identifies the newest call after a reconnect or timeout.
            let priority = (
                if current { 1 + u8::from(active) } else { 0 },
                std::cmp::Reverse(entry["order"].as_u64().unwrap_or(u64::MAX)),
            );
            if priority > group.representative {
                group.run_id = run.into();
                group.tool = super::super::panel::safe_text(field(entry, "tool"));
                group.representative = priority;
            }
            if active {
                group.running += usize::from(current);
                group.unknown |= connected && !current;
            }
        }
        for group in &mut groups {
            if group.running > 0 {
                group.status = if group.unknown {
                    "running + unknown"
                } else {
                    "running"
                }
                .into();
            } else if group.unknown {
                group.status = "activity unknown".into();
            }
        }
        groups.sort_by_key(|group| group.running == 0);
        groups
    }
}

fn field<'a>(value: &'a Value, name: &str) -> &'a str {
    value[name].as_str().unwrap_or("")
}

#[cfg(test)]
mod caller_tests {
    use super::*;
    use serde_json::json;

    fn call(id: &str, active: bool) -> Value {
        json!({"run_id":id,"name":"ChatGPT","model":"model","source":"reported","active":active,"status":"done"})
    }

    #[test]
    fn repeated_calls_group_without_changing_recipient_or_run_identities() {
        let data = json!({"connected":true,"room_id":"one"});
        let mut tracker = CallerTracker::default();
        for run in ["a", "b", "c"] {
            tracker.observe(&json!({"run_id":run,"phase":"started"}), &data);
        }
        let observed = vec![call("a", false), call("b", true), call("c", false)];
        let before = observed.clone();
        let groups = tracker.groups(&observed, &data);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].calls, 3);
        assert_eq!(groups[0].running, 1);
        assert_eq!(groups[0].status, "running");
        assert_eq!(observed, before);
    }

    #[test]
    fn disconnect_and_new_room_do_not_revive_unfinished_historical_calls() {
        let data = json!({"connected":true,"room_id":"one"});
        let mut tracker = CallerTracker::default();
        tracker.observe(&json!({"run_id":"a","phase":"started"}), &data);
        let observed = [call("a", true)];
        assert_eq!(tracker.groups(&observed, &data)[0].running, 1);
        assert_eq!(
            tracker.groups(&observed, &json!({"connected":false,"room_id":"one"}))[0].status,
            "offline"
        );
        let groups = tracker.groups(&observed, &data);
        assert_eq!(groups[0].running, 0);
        assert!(groups[0].unknown);
        tracker.observe(
            &json!({"run_id":"b","phase":"started"}),
            &json!({"connected":true,"room_id":"two"}),
        );
        let groups = tracker.groups(
            &[call("a", true), call("b", true)],
            &json!({"connected":true,"room_id":"two"}),
        );
        assert_eq!(groups[0].running, 1);
        assert!(groups[0].unknown);
    }

    #[test]
    fn explicit_disconnect_between_frames_keeps_replayed_starts_stale() {
        let live = json!({"connected":true,"room_id":"same-room"});
        let mut tracker = CallerTracker::default();
        let start = json!({"phase":"started","run_id":"old"});
        tracker.observe(&start, &live);
        tracker.disconnect();
        tracker.observe(&start, &live);
        let group = tracker.groups(&[call("old", true)], &live).remove(0);
        assert_eq!(group.running, 0);
        assert_eq!(group.status, "activity unknown");
        tracker.observe(&json!({"phase":"started","run_id":"new"}), &live);
        let group = tracker
            .groups(&[call("old", true), call("new", true)], &live)
            .remove(0);
        assert_eq!(group.calls, 2);
        assert_eq!(group.running, 1);
        assert!(group.unknown);
        assert_eq!(group.status, "running + unknown");
        let group = tracker
            .groups(&[call("old", false), call("new", true)], &live)
            .remove(0);
        assert_eq!(group.running, 1);
        assert!(!group.unknown);
    }

    #[test]
    fn remembered_runs_are_bounded_and_missing_starts_never_claim_live_activity() {
        let live = json!({"connected":true,"room_id":"room"});
        let mut tracker = CallerTracker::default();
        for index in 0..300 {
            tracker.observe(
                &json!({"phase":"started","run_id":format!("run-{index}")}),
                &live,
            );
        }
        assert_eq!(tracker.runs.len(), 256);
        tracker.disconnect();
        assert_eq!(tracker.stale.len(), 256);
        let groups = tracker.groups(
            &[
                call("run-0", true),
                call("run-299", true),
                call("unseen", true),
            ],
            &live,
        );
        assert_eq!(groups[0].calls, 3);
        assert_eq!(groups[0].running, 0);
        assert!(groups[0].unknown);
    }

    #[test]
    fn unnamed_callers_and_distinct_models_remain_distinct() {
        let mut tracker = CallerTracker::default();
        let mut second = call("b", false);
        second["model"] = json!("other");
        let mut unknown = call("c", false);
        unknown["name"] = json!("");
        let mut other_unknown = unknown.clone();
        other_unknown["run_id"] = json!("d");
        let groups = tracker.groups(
            &[call("a", false), second, unknown, other_unknown],
            &json!({"preview":true}),
        );
        assert_eq!(groups.len(), 4);
    }

    #[test]
    fn caller_selection_key_survives_new_calls_but_separates_ambiguous_labels() {
        let mut tracker = CallerTracker::default();
        let data = json!({"preview":true});
        let first = tracker.groups(&[call("a", false)], &data).remove(0);
        let updated = tracker
            .groups(&[call("b", true), call("a", false)], &data)
            .remove(0);
        assert_eq!(first.key, updated.key);
        assert_eq!(updated.calls, 2);
        assert_eq!(updated.run_id, "b");
        let mut different_source = call("c", false);
        different_source["source"] = json!("MCP clientInfo");
        assert_ne!(first.key, tracker.groups(&[different_source], &data)[0].key);
        let unknown = |id| json!({"run_id":id,"source":"metadata missing"});
        let groups = tracker.groups(&[unknown("u1"), unknown("u2")], &data);
        assert_ne!(groups[0].key, groups[1].key);
    }

    #[test]
    fn audit_representative_prefers_current_activity_over_stale_starts() {
        let mut tracker = CallerTracker::default();
        let data = json!({"connected":true,"room_id":"current"});
        tracker.observe(&json!({"run_id":"live","phase":"started"}), &data);
        let group = tracker
            .groups(&[call("old", true), call("live", true)], &data)
            .remove(0);
        assert_eq!(group.run_id, "live");
        assert_eq!(group.running, 1);
        assert!(group.unknown);
        let group = tracker
            .groups(&[call("old", true), call("live", false)], &data)
            .remove(0);
        assert_eq!(
            group.run_id, "live",
            "a completed current call replaces the stale one"
        );
        let mut newest = call("newest", false);
        newest["order"] = json!(0);
        let group = tracker
            .groups(&[call("old", true), newest], &json!({"connected":false}))
            .remove(0);
        assert_eq!(
            group.run_id, "newest",
            "offline inspection still opens the newest retained call"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::short_id;

    #[test]
    fn shell_job_suffixes_are_hexadecimal_and_native_ids_remain_distinct() {
        let nonce = "1234567890abcdef1234567890abcdef";
        for suffix in ["9", "a", "c", "f", "10", "ff"] {
            assert_eq!(short_id(&format!("{nonce}-{suffix}")), format!("#{suffix}"));
        }
        assert_eq!(short_id("ordinary-a"), "ordinary-a");
        assert!(!short_id(&format!("{nonce}-not-hex")).starts_with('#'));
    }
}
