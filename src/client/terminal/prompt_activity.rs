//! User-facing timer actions. Retained alarm bodies are published separately by
//! the native scheduler. Never copy templates or inbox text to the activity log.
use super::locale;
use serde_json::Value;

pub(crate) struct PromptActivity {
    key: &'static str,
    unfiltered_read: bool,
    delay_ms: Option<u64>,
    interval_ms: Option<u64>,
}

impl PromptActivity {
    pub(crate) fn new(arguments: &Value) -> Self {
        let action = arguments["action"].as_str();
        let key = match action {
            Some("schedule") => "reminder.schedule",
            Some("pause") => "reminder.pause",
            Some("resume") => "reminder.resume",
            Some("cancel") => "reminder.cancel",
            Some("ack") => "reminder.ack",
            Some("read") => "reminder.read",
            Some("list") => "reminder.list",
            Some("status") => "reminder.status",
            _ => "reminder.unknown",
        };
        Self {
            key,
            unfiltered_read: action == Some("read") && arguments.get("timer_id").is_none(),
            delay_ms: arguments["after_ms"].as_u64(),
            interval_ms: arguments["interval_ms"].as_u64(),
        }
    }

    pub(crate) fn label(&self, outcome: &Result<Value, String>) -> Option<String> {
        let data = outcome
            .as_ref()
            .ok()
            .filter(|reply| reply["isError"] != true)
            .map(|reply| &reply["structuredContent"]);
        // A generic inbox read is the relay's compatibility probe. The clock
        // already published each returned event in its owning conversation.
        if self.unfiltered_read && data.is_some_and(|v| v["events"].is_array()) {
            return None;
        }
        let mut label = locale::text(self.key).to_owned();
        if data.is_some() && self.key == "reminder.schedule" {
            for (key, ms) in [
                ("reminder.after", self.delay_ms),
                ("reminder.every", self.interval_ms),
            ] {
                if let Some(ms) = ms {
                    let (n, unit) = if ms >= 60_000 && ms.is_multiple_of(60_000) {
                        (ms / 60_000, "min")
                    } else if ms >= 1000 && ms.is_multiple_of(1000) {
                        (ms / 1000, "s")
                    } else {
                        (ms, "ms")
                    };
                    label.push_str(&format!(" · {} {n} {unit}", locale::text(key)));
                }
            }
        }
        Some(label)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn automatic_reads_are_quiet_even_on_redelivery_but_failures_and_targeted_reads_are_visible() {
        let read = PromptActivity::new(&json!({"action":"read","limit":4}));
        for events in [json!([]), json!([{"body":"private reminder"}])] {
            assert!(
                read.label(&Ok(json!({"structuredContent":{"events":events}})))
                    .is_none()
            );
        }
        assert!(read.label(&Err("disconnected".into())).is_some());
        assert!(read.label(&Ok(json!({"isError":true}))).is_some());
        let targeted = PromptActivity::new(&json!({"action":"read","timer_id":"prompt_1"}));
        assert!(
            targeted
                .label(&Ok(json!({"structuredContent":{"events":[]}})))
                .is_some()
        );
    }

    #[test]
    fn schedule_labels_explain_the_delay_without_reproducing_private_input() {
        let action = PromptActivity::new(&json!({"action":"schedule","after_ms":60000,
            "interval_ms":5000,"template":"private template","variables":{"secret":"private data"}}));
        let label = action
            .label(&Ok(json!({"structuredContent":{"schedule":{}}})))
            .unwrap();
        assert!(label.contains("1 min") && label.contains("5 s"));
        assert!(!label.contains("private"));
        assert_ne!(
            PromptActivity::new(&json!({"action":"cancel"})).key,
            action.key
        );
    }
}
