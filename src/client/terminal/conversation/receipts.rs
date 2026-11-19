//! Receipt hints share the queue's confirmed state and the editor's actual routing.
use super::*;

impl State {
    pub(super) fn chat_receipt_context(&self, queue: &[Value]) -> Option<String> {
        let pending = queue
            .iter()
            .filter(|entry| Receipt::from_entry(entry).pending())
            .count();
        if pending > 0 {
            // Up acts on the submitting record or newest still-queued record.
            // A pending/uncertain newer item must not advertise editing an older one.
            let latest = if let Some((id, _)) = &self.conversation.submitting {
                queue.iter().find(|entry| message_id(entry) == id)
            } else {
                queue.iter().rev().find(|entry| {
                    !matches!(
                        Receipt::from_entry(entry),
                        Receipt::Read | Receipt::Cancelled | Receipt::Editing
                    )
                })
            };
            let recall = latest.is_some_and(|entry| {
                matches!(
                    Receipt::from_entry(entry),
                    Receipt::Waiting | Receipt::Offline
                )
            }) && self.conversation.composer.is_empty()
                && !self.conversation.content_focus
                && !self.help
                && !self.selection
                && self.conversation.parked.is_none()
                && self.conversation.editing.is_none()
                && self.conversation.recalling.is_none()
                && self.conversation.view != View::Questions
                && !self.chat_menu_visible()
                && !self.conversation.composer.completion_loading();
            let hint = if recall {
                "↑ edit"
            } else if queue
                .iter()
                .any(|entry| Receipt::from_entry(entry).editable())
            {
                "Edit in /queue"
            } else {
                "/queue"
            };
            Some(format!("○ {pending} waiting · {hint}"))
        } else {
            match queue.last().map(Receipt::from_entry) {
                Some(Receipt::Read) => Some("● Read · /queue".into()),
                Some(Receipt::Review | Receipt::Unconfirmed) => Some("○ Review in /queue".into()),
                _ => None,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_composer_hints_follow_editor_focus_and_confirmation() {
        let mut state = super::super::super::preview_state(24, 80, &Ui::default());
        let mut queue = vec![json!({"id":"one","status":"queued"})];
        assert_eq!(
            state.chat_receipt_context(&queue).as_deref(),
            Some("○ 1 waiting · ↑ edit")
        );
        state.conversation.composer.set_text("保留草稿");
        let cursor = state.conversation.composer.cursor();
        assert_eq!(
            state.chat_receipt_context(&queue).as_deref(),
            Some("○ 1 waiting · Edit in /queue")
        );
        assert_eq!(state.conversation.composer.cursor(), cursor);
        state.conversation.composer.set_text("");
        state.conversation.content_focus = true;
        assert!(
            !state
                .chat_receipt_context(&queue)
                .unwrap()
                .contains("↑ edit")
        );
        state.conversation.content_focus = false;
        state.conversation.recalling = Some("one".into());
        assert!(
            !state
                .chat_receipt_context(&queue)
                .unwrap()
                .contains("↑ edit")
        );
        state.conversation.recalling = None;
        queue.push(json!({"id":"two","status":"sending"}));
        assert!(
            !state
                .chat_receipt_context(&queue)
                .unwrap()
                .contains("↑ edit")
        );
        queue[1]["status"] = json!("offline");
        assert!(
            state
                .chat_receipt_context(&queue)
                .unwrap()
                .contains("↑ edit")
        );
        state.selection = true;
        assert!(
            !state
                .chat_receipt_context(&queue)
                .unwrap()
                .contains("↑ edit")
        );
    }

    #[test]
    fn receipt_summary_never_reuses_an_older_read_receipt_for_a_newer_request() {
        let state = super::super::super::preview_state(24, 80, &Ui::default());
        let mut queue = vec![json!({"id":"one","status":"delivered",
            "delivery":{"run_id":"run_one","agent_id":null,"included_at_ms":1000}})];
        assert_eq!(
            state.chat_receipt_context(&queue).as_deref(),
            Some("● Read · /queue")
        );
        queue.push(json!({"id":"two","status":"delivered"}));
        assert_eq!(
            state.chat_receipt_context(&queue).as_deref(),
            Some("○ Review in /queue")
        );
        queue[1]["status"] = json!("cancelled");
        assert!(state.chat_receipt_context(&queue).is_none());
    }
}
