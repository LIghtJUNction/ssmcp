//! User-selected conversation goals; the v1 message log remains unchanged.
use super::*;

pub(crate) const MAX_GOAL_BYTES: usize = 1024;
pub(crate) const MAX_GOAL_CHARS: usize = 256;
const MAX_SIDECAR_BYTES: usize = 16 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Goal {
    schema_version: u32,
    id: String,
    workspace: String,
    text: String,
}

fn name(id: &str) -> Result<String, String> {
    session_name(id)?;
    Ok(format!("{id}.goal.json"))
}

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
        if self.archive_index()?.contains(owner.id()) {
            return Err("history_owner_archived: the original conversation is archived".into());
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
                    "goal_conversation_changed: the original history lease is unavailable".into(),
                );
            }
        }
        Ok(pending)
    }

    fn read_goal(&self, owner: &HistoryOwner) -> Result<Option<String>, String> {
        if self.goal_owner(owner)? {
            return Ok(None);
        }
        let name = name(owner.id())?;
        if !self.directory.names()?.contains(&name) {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        self.directory
            .open(&name, false, false, false)?
            .take(MAX_SIDECAR_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read conversation goal")?;
        if bytes.len() > MAX_SIDECAR_BYTES {
            return Err("conversation goal is oversized".into());
        }
        let goal: Goal = serde_json::from_slice(&bytes).map_err(|_| "invalid conversation goal")?;
        if goal.schema_version != 1 || goal.id != owner.id() || goal.workspace != self.workspace {
            return Err("conversation goal identity or workspace does not match".into());
        }
        normalize(&goal.text).map(Some)
    }

    fn change_goal(
        &mut self,
        owner: &HistoryOwner,
        text: Option<&str>,
    ) -> Result<Option<String>, String> {
        let text = text.map(normalize).transpose()?;
        let _lock = self.directory.lock()?;
        let pending = self.goal_owner(owner)?;
        if pending && text.is_none() {
            return Ok(None);
        }
        let names = self.directory.names()?;
        if pending {
            // Reserve a real, empty conversation. A goal is not a fabricated
            // user message, and this operation must not pick a newer owner.
            if names.contains(&session_name(owner.id())?) {
                return Err("history_owner_conflict: reserved conversation already exists".into());
            }
            let metadata = SessionMetadata {
                schema: SCHEMA.into(),
                schema_version: 1,
                id: owner.id().to_owned(),
                workspace: self.workspace.clone(),
                created_at_ms: now_ms(),
                truncated_messages: 0,
            };
            let lease = self.lease(owner.id())?;
            let conversation = Conversation {
                metadata: metadata.clone(),
                messages: Vec::new(),
            };
            let bytes = Self::encode(&conversation)?;
            self.retain(owner.id(), bytes.len())?;
            self.directory
                .write_atomic(&session_name(owner.id())?, &bytes)?;
            self.ownership.materialized(owner);
            self.active = Some(Active {
                metadata,
                _lease: lease,
            });
        }
        if let Some(text) = &text {
            let goal = Goal {
                schema_version: 1,
                id: owner.id().to_owned(),
                workspace: self.workspace.clone(),
                text: text.clone(),
            };
            let bytes = serde_json::to_vec(&goal).map_err(|_| "cannot encode conversation goal")?;
            if bytes.len() > MAX_SIDECAR_BYTES {
                return Err("encoded conversation goal is oversized".into());
            }
            self.directory.write_atomic(&name(owner.id())?, &bytes)?;
        } else {
            self.remove_goal(owner.id(), &names)?;
        }
        Ok(text)
    }

    pub(super) fn remove_goal(&self, id: &str, names: &[String]) -> Result<(), String> {
        let name = name(id)?;
        if names.contains(&name) {
            self.directory.remove(&name)?;
        }
        Ok(())
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
