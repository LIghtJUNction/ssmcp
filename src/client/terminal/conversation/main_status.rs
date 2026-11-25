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
        .max_by_key(|state| (state.rank, state.connected_ms))
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
        "Offline" | "Retiring" | "Dreaming" => (rank, lifecycle),
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
    let connected_ms = (label != "Offline")
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

pub(super) fn header(context: &str, view: &str, status: &str, width: u16) -> (String, String) {
    let padding = if width >= 32 { "  " } else { "" };
    let label = if width >= 40 {
        let view = fit(view, width.saturating_sub(status.width() as u16 + 20));
        format!("{status} · {view}{padding}")
    } else {
        format!("{status}{padding}")
    };
    let right = fit(&label, width);
    let available = width.saturating_sub(right.width() as u16 + 1);
    let title = if width >= 32 {
        fit(&format!("{padding}SSMCP main{context}"), available)
    } else if available >= 10 {
        "SSMCP main".into()
    } else if available >= 5 {
        "SSMCP".into()
    } else {
        String::new()
    };
    let gap = usize::from(width).saturating_sub(title.width() + right.width());
    (title, format!("{}{right}", " ".repeat(gap)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roster_summary_uses_only_local_coordinator_members() {
        let mut data = json!({"connected":true,"supported":true,"session_id":"this-room",
            "agents":[{"role":"worker","status":"working","dreaming_active":true}]});
        assert_eq!(
            from_roster(
                &data,
                "this-room",
                false,
                &Default::default(),
                &Jobs::default()
            )
            .label,
            "Unknown"
        );
        let agents = data["agents"].as_array_mut().unwrap();
        agents.push(json!({"role":"main","status":"offline"}));
        agents.push(json!({"role":"lead","status":"idle"}));
        assert_eq!(
            from_roster(
                &data,
                "this-room",
                false,
                &Default::default(),
                &Jobs::default()
            )
            .label,
            "Idle"
        );
        data["agents"][1]["status"] = json!("future_status");
        assert_eq!(
            from_roster(
                &data,
                "this-room",
                false,
                &Default::default(),
                &Jobs::default()
            )
            .label,
            "Unknown"
        );
        data["agents"][2]["status"] = json!("working");
        data["agents"][2]["activity"] =
            json!({"phase":"work","observed_age_ms":0,"connected_ms":1});
        assert_eq!(
            from_roster(
                &data,
                "this-room",
                false,
                &Default::default(),
                &Jobs::default()
            )
            .label,
            "Working"
        );
        data["reloading"] = json!(true);
        assert_eq!(
            from_roster(
                &data,
                "this-room",
                false,
                &Default::default(),
                &Jobs::default()
            )
            .label,
            "Unknown"
        );
        assert_eq!(
            from_roster(
                &data,
                "this-room",
                true,
                &Default::default(),
                &Jobs::default()
            )
            .label,
            "Offline"
        );
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
            let (_, right) = header("", "conversation", &open.badge(width), width);
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
