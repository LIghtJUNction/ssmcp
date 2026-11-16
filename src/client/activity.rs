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
