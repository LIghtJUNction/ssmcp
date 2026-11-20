//! Reversible conversation archiving. The original JSONL is never rewritten.
use super::*;
use std::collections::BTreeSet;

const INDEX_NAME: &str = "archives.json";

#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Filter {
    #[default]
    Active,
    Archived,
    All,
}

impl Filter {
    pub(super) fn includes(self, archived: bool) -> bool {
        match self {
            Self::Active => !archived,
            Self::Archived => archived,
            Self::All => true,
        }
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Index {
    schema_version: u32,
    conversations: BTreeSet<String>,
}

impl Index {
    pub(super) fn contains(&self, id: &str) -> bool {
        self.conversations.contains(id)
    }
}

impl History {
    // Caller owns the directory lock, so the JSONL and marker are one snapshot.
    pub(super) fn archive_index(&self) -> Result<Index, String> {
        if !self
            .directory
            .names()?
            .iter()
            .any(|name| name == INDEX_NAME)
        {
            return Ok(Index {
                schema_version: 1,
                conversations: BTreeSet::new(),
            });
        }
        let mut bytes = Vec::new();
        self.directory
            .open(INDEX_NAME, false, false, false)?
            .take(MAX_PAGE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read conversation archive index")?;
        if bytes.len() > MAX_PAGE_BYTES {
            return Err("conversation archive index is oversized".into());
        }
        let index: Index =
            serde_json::from_slice(&bytes).map_err(|_| "invalid conversation archive index")?;
        if index.schema_version != 1
            || index.conversations.len() > MAX_SESSIONS
            || index
                .conversations
                .iter()
                .any(|id| !valid_id(id, "conversation"))
        {
            return Err("unsupported conversation archive index".into());
        }
        Ok(index)
    }

    pub(super) fn archive_change(
        &mut self,
        id: Option<&str>,
        archived: bool,
    ) -> Result<Value, String> {
        let _lock = self.directory.lock()?;
        let id = id
            .map(str::to_owned)
            .or_else(|| {
                self.active
                    .as_ref()
                    .map(|active| active.metadata.id.clone())
            })
            .ok_or("no current conversation; select a saved conversation first")?;
        let conversation = self.load(&id)?;
        if conversation.metadata.workspace != self.workspace {
            return Err("conversation belongs to a different workspace".into());
        }
        let current = self
            .active
            .as_ref()
            .is_some_and(|active| active.metadata.id == id);
        // Never archive an actively written conversation owned by another client.
        let _lease = if current {
            None
        } else {
            Some(self.lease(&id)?)
        };
        let mut index = self.archive_index()?;
        let changed = if archived {
            index.conversations.insert(id.clone())
        } else {
            index.conversations.remove(&id)
        };
        if index.conversations.len() > MAX_SESSIONS {
            return Err(
                "conversation archive capacity is full; restore an archived conversation first"
                    .into(),
            );
        }
        if changed {
            let bytes = serde_json::to_vec(&index)
                .map_err(|_| "cannot encode conversation archive index")?;
            self.directory.write_atomic(INDEX_NAME, &bytes)?;
        }
        if archived {
            self.ownership.revoke(&id);
        }
        if archived && current {
            // New messages start a new history, leaving the archived JSONL intact.
            self.active = None;
        }
        Ok(json!({
            "schema_version":1, "id":id, "archived":archived, "changed":changed,
            "current":current && !archived, "message_count":conversation.messages.len(),
            "contents_changed":false
        }))
    }
}

pub(crate) fn change(id: Option<&str>, archived: bool) -> Result<Value, String> {
    with(|history| {
        let selected = history.ownership.current.get().is_some();
        let result = history.archive_change(id, archived)?;
        if selected && history.ownership.current.get().is_none() {
            super::super::messages::restore_copy(&[]);
        }
        Ok(result)
    })
}

pub(crate) fn sessions(filter: Filter) -> Result<Vec<SessionSummary>, String> {
    with(|history| history.sessions_filtered(filter))
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    Archive,
    Restore,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    action: Action,
    id: String,
}

pub(crate) fn call(arguments: Value) -> Result<Value, String> {
    let request: Request = serde_json::from_value(arguments)
        .map_err(|_| "history_archive requires archive/restore and a conversation id")?;
    session_name(&request.id)?;
    change(Some(&request.id), matches!(request.action, Action::Archive))
}

pub(crate) fn tool() -> Value {
    json!({
        "name":"ssmcp.ui.history_archive",
        "description":"Archive or restore one saved conversation in the current workspace. Archiving hides it from the default history list and protects it from automatic retention cleanup; it does not rewrite/delete messages or stop jobs. Archiving the current conversation closes that history so later messages start a new one. Restore makes it visible again without selecting it or replaying requests. Both operations are idempotent. History remains bounded by its existing 64-session/16-MiB budget; a full set of active or archived histories blocks new history writes instead of deleting protected logs.",
        "inputSchema":{
            "type":"object","additionalProperties":false,"required":["action","id"],
            "properties":{
                "action":{"type":"string","enum":["archive","restore"]},
                "id":{"type":"string","pattern":"^conversation_[0-9a-f]{32}$"}
            }
        },
        "annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true}
    })
}

#[cfg(test)]
mod tests;
