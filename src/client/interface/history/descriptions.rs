//! Editable descriptions live beside the original, immutable-format conversation JSONL.
use super::{History, SessionSummary, safe_text, session_name};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::io::Read;

const MAX_TITLE_BYTES: usize = 256;
const MAX_SUMMARY_BYTES: usize = 2048;
const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Description {
    schema_version: u32,
    id: String,
    workspace: String,
    title: Option<String>,
    summary: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
    title: Option<String>,
    summary: Option<String>,
}

impl Request {
    fn parse(arguments: Value) -> Result<Self, String> {
        if arguments
            .as_object()
            .is_none_or(|object| object.values().any(Value::is_null))
        {
            return Err(
                "history_update expects an object; omit optional fields instead of null".into(),
            );
        }
        let request: Self = serde_json::from_value(arguments)
            .map_err(|_| "history_update requires id and optional title/summary strings")?;
        session_name(&request.id)?;
        if request.title.is_none() && request.summary.is_none() {
            return Err("provide title and/or summary; an empty string clears that field".into());
        }
        Ok(request)
    }
}

fn name(id: &str) -> Result<String, String> {
    session_name(id)?;
    Ok(format!("{id}.description.json"))
}

fn normalize(value: &str, label: &str, limit: usize) -> Result<Option<String>, String> {
    if value.len() > limit || value.chars().any(char::is_control) {
        return Err(format!(
            "{label} must contain no control characters and fit {limit} UTF-8 bytes"
        ));
    }
    if value.is_empty() {
        return Ok(None);
    }
    let mut redacted = safe_text(value);
    // A redaction marker can be longer than the supplied secret.
    redacted.truncate(redacted.floor_char_boundary(limit));
    Ok(Some(redacted))
}

impl Description {
    fn apply(self, summary: &mut SessionSummary) {
        if let Some(title) = self.title {
            summary.title = title;
        }
        summary.summary = self.summary;
    }
}

impl History {
    fn description(&self, id: &str, names: &[String]) -> Result<Option<Description>, String> {
        let name = name(id)?;
        if !names.contains(&name) {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        self.directory
            .open(&name, false, false, false)?
            .take(MAX_DESCRIPTION_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "cannot read conversation description")?;
        if bytes.len() > MAX_DESCRIPTION_BYTES {
            return Err("conversation description is oversized".into());
        }
        let mut description: Description =
            serde_json::from_slice(&bytes).map_err(|_| "invalid conversation description")?;
        if description.schema_version != 1
            || description.id != id
            || description.workspace != self.workspace
        {
            return Err("conversation description identity or workspace does not match".into());
        }
        description.title = description
            .title
            .map(|value| normalize(&value, "title", MAX_TITLE_BYTES))
            .transpose()?
            .flatten();
        description.summary = description
            .summary
            .map(|value| normalize(&value, "summary", MAX_SUMMARY_BYTES))
            .transpose()?
            .flatten();
        Ok(Some(description))
    }

    pub(super) fn describe_summary(&self, summary: &mut SessionSummary, names: &[String]) {
        // An unavailable sidecar must not hide the underlying conversation ID.
        if let Ok(Some(description)) = self.description(&summary.id, names) {
            description.apply(summary);
        }
    }

    pub(super) fn remove_description(&self, id: &str, names: &[String]) -> Result<(), String> {
        let name = name(id)?;
        if names.contains(&name) {
            self.directory.remove(&name)?;
        }
        Ok(())
    }

    fn change_description(&mut self, request: Request) -> Result<Value, String> {
        let title = request
            .title
            .as_deref()
            .map(|value| normalize(value, "title", MAX_TITLE_BYTES))
            .transpose()?;
        let summary = request
            .summary
            .as_deref()
            .map(|value| normalize(value, "summary", MAX_SUMMARY_BYTES))
            .transpose()?;
        let _lock = self.directory.lock()?;
        let conversation = self.load(&request.id)?;
        if conversation.metadata.workspace != self.workspace {
            return Err("conversation belongs to a different workspace".into());
        }
        let current = self
            .active
            .as_ref()
            .is_some_and(|active| active.metadata.id == request.id);
        let _lease = if current {
            None
        } else {
            Some(self.lease(&request.id)?)
        };
        let archived = self.archive_index()?.contains(&request.id);
        let names = self.directory.names()?;
        let mut description =
            self.description(&request.id, &names)?
                .unwrap_or_else(|| Description {
                    schema_version: 1,
                    id: request.id.clone(),
                    workspace: self.workspace.clone(),
                    title: None,
                    summary: None,
                });
        let mut changed = false;
        if let Some(title) = title {
            changed |= description.title != title;
            description.title = title;
        }
        if let Some(summary) = summary {
            changed |= description.summary != summary;
            description.summary = summary;
        }
        if changed {
            let bytes = serde_json::to_vec(&description)
                .map_err(|_| "cannot encode conversation description")?;
            if bytes.len() > MAX_DESCRIPTION_BYTES {
                return Err("encoded conversation description is oversized".into());
            }
            self.directory.write_atomic(&name(&request.id)?, &bytes)?;
        }
        let mut result = self.summary(&conversation);
        description.apply(&mut result);
        result.archived = archived;
        Ok(json!({
            "schema_version":1, "id":request.id, "changed":changed,
            "contents_changed":false, "summary":result
        }))
    }
}

pub(crate) fn call(arguments: Value) -> Result<Value, String> {
    let request = Request::parse(arguments)?;
    let result = super::with(|history| history.change_description(request))?;
    if result["changed"] == true {
        crate::client::terminal::history_description_changed();
    }
    Ok(result)
}

pub(crate) fn tool() -> Value {
    json!({
        "name":"ssmcp.ui.history_update",
        "description":"Update one saved conversation's title and/or summary in the current workspace. Supply an exact conversation ID from ssmcp.ui.history. Omit a field to keep its value. An empty string clears the field and restores the automatic title if you clear title. This saves only the redacted description in a private sidecar. Editing requires a conversation that this client holds or that other clients have closed.",
        "inputSchema":{
            "type":"object","additionalProperties":false,"required":["id"],
            "anyOf":[{"required":["title"]},{"required":["summary"]}],
            "properties":{
                "id":{"type":"string","pattern":"^conversation_[0-9a-f]{32}$"},
                "title":{"type":"string","maxLength":MAX_TITLE_BYTES,"description":"At most 256 UTF-8 bytes. Control characters are rejected. Empty clears."},
                "summary":{"type":"string","maxLength":MAX_SUMMARY_BYTES,"description":"A natural-language summary of at most 2048 UTF-8 bytes. Control characters are rejected. Empty clears."}
            }
        },
        "annotations":{"readOnlyHint":false,"destructiveHint":false,"idempotentHint":true}
    })
}

#[cfg(test)]
mod tests;
