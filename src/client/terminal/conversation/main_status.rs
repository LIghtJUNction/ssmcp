//! Local coordinator activity for the main header. Worker pool and messaging
//! presence belong to their own views.
use super::*;

pub(super) fn from_roster(data: &Value, current_session: &str, disconnected: bool) -> &'static str {
    if disconnected {
        return "Offline";
    }
    if data["connected"] != true
        || data["supported"] != true
        || data["chat_enabled"] == false
        || data["reloading"] == true
        || (!current_session.is_empty() && data["session_id"] != current_session)
    {
        return "Unknown";
    }
    // Multiple coordinators can share one session. Active work takes precedence;
    // Offline requires every known coordinator to be offline.
    data["agents"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|member| {
            member["role"].as_str().is_some_and(|role| {
                ["main", "coordinator", "controller", "lead"]
                    .iter()
                    .any(|known| role.eq_ignore_ascii_case(known))
            })
        })
        .map(member_lifecycle)
        .max()
        .map_or("Unknown", |(_, label)| label)
}

pub(super) fn member_lifecycle(member: &Value) -> (u8, &'static str) {
    match (
        member["status"].as_str(),
        member["dreaming_active"].as_bool(),
    ) {
        (Some("working"), _) => (6, "Working"),
        (Some("dreaming" | "idle"), Some(true)) => (5, "Dreaming"),
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
        assert_eq!(from_roster(&data, "this-room", false), "Unknown");
        let agents = data["agents"].as_array_mut().unwrap();
        agents.push(json!({"role":"main","status":"offline"}));
        agents.push(json!({"role":"lead","status":"idle"}));
        assert_eq!(from_roster(&data, "this-room", false), "Idle");
        data["agents"][1]["status"] = json!("future_status");
        assert_eq!(from_roster(&data, "this-room", false), "Unknown");
        data["agents"][2]["status"] = json!("working");
        assert_eq!(from_roster(&data, "this-room", false), "Working");
        data["reloading"] = json!(true);
        assert_eq!(from_roster(&data, "this-room", false), "Unknown");
        assert_eq!(from_roster(&data, "this-room", true), "Offline");
    }
}
