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
    if member["activity_status"] == "offline" {
        return (0, "Offline");
    }
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
pub(super) fn header(state: &State, view: &str, status: &str, width: u16) -> String {
    let center = fit(status, width);
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
    let tone = match status {
        "Working" => PanelTone::Success,
        "Idle" => PanelTone::Normal,
        "Dreaming" => PanelTone::Accent,
        "Retiring" => PanelTone::Warning,
        "Offline" => PanelTone::Error,
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
