//! Local coordinator activity for the main header. Worker pool and messaging
//! presence belong to their own views.
use super::*;

#[derive(Default)]
pub(in crate::client::terminal) struct Jobs(BTreeMap<(u64, String), String>);

impl Jobs {
    pub(in crate::client::terminal) fn observe(
        &mut self,
        room: &str,
        label: &str,
        event: &Value,
        source: &crate::runtime::call_scope::DisplaySource,
    ) {
        let Some(call) = &source.call else {
            return;
        };
        if label != "shell"
            || call.client_id() != Some(room)
            || !matches!(
                call.native_tool(),
                Some("ssmcp.exec.start" | "terminal.open")
            )
        {
            return;
        }
        let Some(agent) = call.attribution().and_then(|a| a.agent.as_ref()) else {
            return;
        };
        let Some(id) = event["job_id"]
            .as_str()
            .filter(|id| !id.is_empty() && id.len() <= 128)
        else {
            return;
        };
        match event["type"].as_str() {
            Some("exec.started") if event["running"] == true && self.0.len() < 64 => {
                self.0
                    .insert((call.connection(), id.to_owned()), agent.id.clone());
            }
            Some("exec.exited") => {
                self.0.remove(&(call.connection(), id.to_owned()));
            }
            _ => {}
        }
    }
}

const FRESH_ROSTER_MS: u64 = 15_000;
const RECENT_WORK_MS: u64 = 60_000;

#[derive(Clone, Copy)]
pub(super) struct Headline {
    rank: u8,
    pub(super) label: &'static str,
    connected_ms: Option<u64>,
}

impl Headline {
    fn state(rank: u8, label: &'static str) -> Self {
        Self {
            rank,
            label,
            connected_ms: None,
        }
    }

    pub(super) fn badge(self, width: u16) -> String {
        let Some(ms) = self.connected_ms else {
            return self.label.into();
        };
        let seconds = ms / 1000;
        let hours = seconds / 3600;
        let duration = if hours > 0 {
            format!("{hours}h{:02}m", seconds / 60 % 60)
        } else if seconds >= 60 {
            format!("{}m", seconds / 60)
        } else {
            format!("{seconds}s")
        };
        if width < 28 {
            let label = match self.label {
                "Job running" => "Job",
                "Tool open" => "Tool",
                label => label,
            };
            return match hours {
                0 | 1 => format!("{label} {duration}"),
                2 => format!("{label} 2h /invite"),
                _ => format!("{label} {hours}h! /invite"),
            };
        }
        let hint = match (hours, width) {
            (0 | 1, _) => "",
            (2, 65..) => " · consider handoff /invite",
            (2, _) => " · /invite",
            (_, 65..) => " · handoff recommended /invite",
            (_, _) => " ! /invite",
        };
        if width < 44 && hours >= 2 {
            format!("{} {duration}{hint}", self.label)
        } else {
            format!("{} · linked {duration}{hint}", self.label)
        }
    }
}

pub(super) fn from_roster(
    data: &Value,
    current_session: &str,
    disconnected: bool,
    timing: &crate::runtime::time::Snapshot,
    jobs: &Jobs,
) -> Headline {
    if disconnected {
        return Headline::state(0, "Offline");
    }
    if data["connected"] != true
        || data["supported"] != true
        || data["chat_enabled"] == false
        || data["reloading"] == true
        || (!current_session.is_empty() && data["session_id"] != current_session)
    {
        return Headline::state(2, "Unknown");
    }
    let roster_age = data["roster_age_ms"].as_u64().unwrap_or(0);
    data["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|member| crate::wire::coordinator_role(member["role"].as_str()))
        .map(|member| for_member(member, roster_age, timing, jobs))
        .max_by_key(|state| (state.rank, state.connected_ms, state.label))
        .unwrap_or(Headline::state(2, "Unknown"))
}

pub(super) fn for_member(
    member: &Value,
    roster_age: u64,
    timing: &crate::runtime::time::Snapshot,
    jobs: &Jobs,
) -> Headline {
    let id = member["agent_id"].as_str();
    let waiting = id.is_some_and(|id| {
        timing.active.iter().any(|entry| {
            entry.durable.is_none()
                && entry
                    .wait
                    .as_ref()
                    .is_some_and(|wait| wait.outcome == crate::wire::wait_display::Outcome::Pending)
                && entry.remaining_ms != Some(0)
                && entry
                    .attribution
                    .as_ref()
                    .and_then(|a| a.agent.as_ref())
                    .is_some_and(|agent| agent.id == id)
        })
    });
    let job = id.is_some_and(|id| jobs.0.values().any(|owner| owner == id));
    coordinator(member, roster_age, waiting, job)
}

fn coordinator(member: &Value, roster_age: u64, relay_wait: bool, job: bool) -> Headline {
    if roster_age >= FRESH_ROSTER_MS {
        return Headline::state(3, "Unknown");
    }
    let (rank, lifecycle) = member_lifecycle(member);
    let activity = &member["activity"];
    let age = activity["observed_age_ms"]
        .as_u64()
        .map(|ms| ms.saturating_add(roster_age));
    let recent = age.is_some_and(|ms| ms < RECENT_WORK_MS);
    let phase = activity["phase"].as_str();
    let (rank, label) = match lifecycle {
        "Offline" | "Offline ☾" | "Retiring" | "Dreaming" => (rank, lifecycle),
        _ if phase == Some("work") && recent => (12, "Working"),
        _ if phase == Some("work") => (11, "Tool open"),
        _ if relay_wait || phase == Some("wait") => (10, "Waiting"),
        _ if job => (9, "Job running"),
        "Working" => match phase {
            Some("recent") if recent => (9, "Recent"),
            Some("recent" | "quiet") => (6, "Quiet"),
            _ => (rank, "Unknown"),
        },
        _ => (rank, lifecycle),
    };
    let connected_ms = (!matches!(label, "Offline" | "Offline ☾"))
        .then(|| activity["connected_ms"].as_u64())
        .flatten()
        .map(|ms| ms.saturating_add(roster_age));
    Headline {
        rank,
        label,
        connected_ms,
    }
}

pub(super) fn member_lifecycle(member: &Value) -> (u8, &'static str) {
    if member["activity_status"] == "offline" {
        let dreaming = matches!(member["status"].as_str(), Some("dreaming" | "idle"))
            && member["dreaming_active"] == true;
        return (0, if dreaming { "Offline ☾" } else { "Offline" });
    }
    match (
        member["status"].as_str(),
        member["dreaming_active"].as_bool(),
    ) {
        (Some("working"), _) => (4, "Working"),
        (Some("dreaming" | "idle"), Some(true)) => (8, "Dreaming"),
        (Some("dreaming"), Some(false)) => (3, "Pending"),
        (Some("idle"), _) => (2, "Idle"),
        (Some("retiring"), _) => (1, "Retiring"),
        (Some("offline"), _) => (0, "Offline"),
        _ => (4, "Unknown"),
    }
}

fn title(context: &str, limit: u16) -> String {
    let brand = "SSMCP main";
    if usize::from(limit) < brand.len() {
        return if limit >= 5 { "SSMCP" } else { "" }.into();
    }
    let full = format!("{brand}{context}");
    if full.width() <= usize::from(limit) {
        return full;
    }
    let prefix = fit(&full, limit - 1);
    let prefix = prefix.trim_end().trim_end_matches("  /").trim_end();
    if prefix.len() <= brand.len() {
        brand.into()
    } else {
        format!("{prefix}…")
    }
}

/// Anchor the status in the middle of the conversation pane, independently of
/// the title and view. An odd spare cell belongs to the right side.
pub(super) fn header(state: &State, view: &str, status: Headline, width: u16) -> String {
    let center = fit(&status.badge(width), width);
    let remaining = usize::from(width) - center.width();
    let left_width = remaining / 2;
    let right_width = remaining - left_width;
    let margin = if width >= 40 { 2 } else { 0 };
    // Reserve a separator next to the status even after a long title is clipped.
    let left_limit = left_width.saturating_sub(margin + 1);
    let title = title(&state.context_label, left_limit as u16);
    let right_limit = right_width.saturating_sub(margin + 1);
    let view = if view == "conversation" && view.len() > right_limit {
        String::new()
    } else {
        fit(view, right_limit as u16)
    };
    let tone = match status.label {
        "Working" => PanelTone::Success,
        "Idle" => PanelTone::Normal,
        "Dreaming" => PanelTone::Accent,
        "Retiring" => PanelTone::Warning,
        "Offline" | "Offline ☾" => PanelTone::Error,
        _ => PanelTone::Muted,
    };
    let left_gap = left_width.saturating_sub(margin + title.width());
    let right_gap = right_width.saturating_sub(view.width() + margin);
    format!(
        "{}{}{}",
        state.tone(
            PanelTone::Heading,
            &format!("{:margin$}{title}{:left_gap$}", "", "")
        ),
        state.tone(tone, &center),
        state.tone(
            PanelTone::Muted,
            &format!("{:right_gap$}{view}{:margin$}", "", "")
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_summary_uses_only_local_coordinator_members() {
        let label = |data: &Value, disconnected| {
            from_roster(
                data,
                "this-room",
                disconnected,
                &Default::default(),
                &Jobs::default(),
            )
            .label
        };
        let mut data = json!({"connected":true,"supported":true,"session_id":"this-room",
            "agents":[{"role":"worker","status":"working","dreaming_active":true}]});
        assert_eq!(label(&data, false), "Unknown");
        let agents = data["agents"].as_array_mut().unwrap();
        agents.push(json!({"role":"main","status":"offline"}));
        agents.push(json!({"role":"lead","status":"idle"}));
        assert_eq!(label(&data, false), "Idle");
        data["agents"][1]["status"] = json!("future_status");
        assert_eq!(label(&data, false), "Unknown");
        data["agents"][2]["status"] = json!("working");
        data["agents"][2]["activity"] =
            json!({"phase":"work","observed_age_ms":0,"connected_ms":1});
        assert_eq!(label(&data, false), "Working");
        data["agents"][1] = json!({"role":"main","status":"idle","dreaming_active":true,"activity_status":"offline"});
        data["agents"][2] = json!({"role":"lead","status":"working","activity_status":"offline"});
        assert_eq!(label(&data, false), "Offline ☾");
        data["agents"][1]["dreaming_active"] = json!(false);
        assert_eq!(label(&data, false), "Offline");
        data["reloading"] = json!(true);
        assert_eq!(label(&data, false), "Unknown");
        assert_eq!(label(&data, true), "Offline");
    }

    #[test]
    fn main_activity_age_thresholds_and_clock_drift_do_not_imply_progress() {
        let mut member = json!({"role":"main", "status":"working","activity":{"connected_ms":7_199_000,"phase":"recent","observed_age_ms":59_000}});
        let before = coordinator(&member, 999, false, false);
        assert_eq!(before.label, "Recent");
        assert!(!before.badge(92).contains("/invite"));
        let after = coordinator(&member, 1000, false, false);
        assert_eq!(after.label, "Quiet");
        assert!(
            after
                .badge(92)
                .contains("linked 2h00m · consider handoff /invite")
        );
        member["activity"]["connected_ms"] = json!(10_800_000);
        member["activity"]["phase"] = json!("work");
        member["activity"]["observed_age_ms"] = json!(60_000);
        let open = coordinator(&member, 0, false, false);
        assert_eq!(open.label, "Tool open");
        assert!(open.badge(92).contains("handoff recommended /invite"));
        for width in [20, 32, 48, 100] {
            let right = open.badge(width);
            assert!(
                right.contains("/invite") && right.width() <= usize::from(width),
                "{right}"
            );
        }
        member["activity"]["phase"] = json!("quiet");
        assert_eq!(coordinator(&member, 0, false, true).label, "Job running");
        assert_eq!(coordinator(&member, 0, true, true).label, "Waiting");
        assert_eq!(
            coordinator(&member, FRESH_ROSTER_MS, false, true).label,
            "Unknown"
        );
        member["status"] = json!("offline");
        assert_eq!(coordinator(&member, 0, true, true).badge(100), "Offline");
    }

    #[test]
    fn main_activity_jobs_require_the_native_creator_and_end_on_its_receipt() {
        use crate::runtime::call_scope::{CallScope, DisplaySource};
        use crate::wire::{ControlAccount, ControlAgent, ControlAttribution};
        let actor = ControlAttribution {
            account: ControlAccount {
                provider: "github".into(),
                subject: "1".into(),
                login: "alice".into(),
            },
            owner: None,
            agent: Some(ControlAgent {
                id: format!("agent_{}", "a".repeat(32)),
                name: "main".into(),
            }),
        };
        let scope =
            CallScope::from_relay_for_client(4, Some("room"), 41, Some(&actor), None).unwrap();
        scope.set_native_tool("ssmcp.exec.start");
        let source = DisplaySource {
            call: Some(scope),
            ..Default::default()
        };
        let mut jobs = Jobs::default();
        let started = json!({"type":"exec.started","job_id":"long","running":true});
        jobs.observe("room", "shell", &started, &DisplaySource::default());
        jobs.observe("room", "plugin", &started, &source);
        jobs.observe("other", "shell", &started, &source);
        assert!(jobs.0.is_empty());
        jobs.observe("room", "shell", &started, &source);
        assert_eq!(jobs.0.len(), 1);
        jobs.observe(
            "room",
            "shell",
            &json!({"type":"exec.exited","job_id":"long"}),
            &source,
        );
        assert!(jobs.0.is_empty());
    }
}
