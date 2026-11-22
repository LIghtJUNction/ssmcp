//! User-selected goals for the current process only.
use super::*;

pub(crate) const MAX_GOAL_BYTES: usize = 1024;
pub(crate) const MAX_GOAL_CHARS: usize = 256;

pub(crate) fn normalize(text: &str) -> Result<String, String> {
    if text.len() > MAX_GOAL_BYTES
        || text.chars().count() > MAX_GOAL_CHARS
        || text
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
    {
        return Err(format!(
            "Goal must be one line, at most {MAX_GOAL_CHARS} characters / {MAX_GOAL_BYTES} UTF-8 bytes"
        ));
    }
    let text = safe_text(text.trim());
    if text.is_empty() {
        return Err("Use /goal set <text> to set a goal, or /goal clear to remove it".into());
    }
    if text.len() > MAX_GOAL_BYTES || text.chars().count() > MAX_GOAL_CHARS {
        return Err("Redacted goal is too long; use a shorter goal".into());
    }
    Ok(text)
}

impl History {
    fn goal_owner(&self, owner: &HistoryOwner) -> Result<bool, String> {
        let pending = self.ownership.pending(owner)?;
        if self.ownership.current.get().as_ref() != Some(owner) {
            return Err(
                "goal_conversation_changed: select the original conversation and retry".into(),
            );
        }
        if !pending {
            let conversation = self.load(owner.id())?;
            if conversation.metadata.workspace != self.workspace
                || self
                    .active
                    .as_ref()
                    .is_none_or(|active| active.metadata.id != owner.id())
            {
                return Err(
                    "goal_conversation_changed: the original live conversation is unavailable"
                        .into(),
                );
            }
        }
        Ok(pending)
    }

    fn read_goal(&self, owner: &HistoryOwner) -> Result<Option<String>, String> {
        if self.goal_owner(owner)? {
            return Ok(None);
        }
        self.goals
            .get(owner.id())
            .map(|text| normalize(text))
            .transpose()
    }

    fn change_goal(
        &mut self,
        owner: &HistoryOwner,
        text: Option<&str>,
    ) -> Result<Option<String>, String> {
        let text = text.map(normalize).transpose()?;
        let pending = self.goal_owner(owner)?;
        if pending && text.is_none() {
            return Ok(None);
        }
        if pending {
            self.materialize(owner)?;
        }
        if let Some(text) = &text {
            self.goals.insert(owner.id().to_owned(), text.clone());
        } else {
            self.goals.remove(owner.id());
        }
        Ok(text)
    }
}

pub(crate) fn read(owner: &HistoryOwner) -> Result<Option<String>, String> {
    ownership::with_owner(owner, |history, owner| history.read_goal(owner))
}

pub(crate) fn change(owner: &HistoryOwner, text: Option<&str>) -> Result<Option<String>, String> {
    ownership::with_owner(owner, |history, owner| history.change_goal(owner, text))
}

#[cfg(test)]
mod tests;
