//! Caller observations retained only in the owning process's live context.
use super::{History, HistoryOwner, ownership::with_owner};
use crate::wire::openai::{self, OpenAiContext};
use reqwest::Url;
use serde::{Deserialize, Serialize};

const MAX_OBSERVATION_BYTES: usize = 16 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Observed {
    pub(crate) value: String,
    pub(crate) observed_ms: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Saved {
    schema_version: u32,
    id: String,
    workspace: String,
    pub(crate) session: Option<Observed>,
    pub(crate) subject: Option<Observed>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) chatgpt_url: Option<String>,
}

const LINK_HINT: &str = "Use an HTTPS ChatGPT conversation URL from chatgpt.com or chat.openai.com (/c/ID or /g/ID/c/ID)";

pub(crate) fn validate_chatgpt_url(raw: &str) -> Result<Url, String> {
    if raw.len() > 2048
        || raw.contains('\\')
        || raw.chars().any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err(LINK_HINT.into());
    }
    let url = Url::parse(raw).map_err(|_| LINK_HINT)?;
    let (authority, path_and_suffix) = raw
        .split_once("://")
        .and_then(|(_, rest)| rest.split_once('/'))
        .ok_or(LINK_HINT)?;
    let host = url.host_str().ok_or(LINK_HINT)?;
    let supplied_path = path_and_suffix.split(['?', '#']).next().ok_or(LINK_HINT)?;
    if url.scheme() != "https"
        || !matches!(host, "chatgpt.com" | "chat.openai.com")
        || !(authority.eq_ignore_ascii_case(host)
            || authority
                .strip_suffix(":443")
                .is_some_and(|name| name.eq_ignore_ascii_case(host)))
        || url.port().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path().strip_prefix('/') != Some(supplied_path)
    {
        return Err(LINK_HINT.into());
    }
    let id = |segment: &str| {
        !segment.is_empty()
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    };
    let segments: Vec<_> = supplied_path.split('/').collect();
    match segments.as_slice() {
        ["c", conversation] if id(conversation) => Ok(url),
        ["g", project, "c", conversation] if id(project) && id(conversation) => Ok(url),
        _ => Err(LINK_HINT.into()),
    }
}

fn update(field: &mut Option<Observed>, value: Option<&str>, observed_ms: u64) -> bool {
    if let Some(value) = value.filter(|value| openai::valid(value))
        && field
            .as_ref()
            .is_none_or(|saved| observed_ms > saved.observed_ms)
    {
        *field = Some(Observed {
            value: value.into(),
            observed_ms,
        });
        return true;
    }
    false
}

impl History {
    fn openai_state(&self, id: &str) -> Result<Saved, String> {
        let conversation = self.load(id)?;
        if conversation.metadata.workspace != self.workspace {
            return Err("The original conversation belongs to a different workspace".into());
        }
        let saved = self.openai.get(id).cloned().unwrap_or_else(|| Saved {
            schema_version: 1,
            id: id.into(),
            workspace: self.workspace.clone(),
            session: None,
            subject: None,
            chatgpt_url: None,
        });
        if saved.schema_version != 1
            || saved.id != id
            || saved.workspace != self.workspace
            || [&saved.session, &saved.subject]
                .into_iter()
                .flatten()
                .any(|field| !openai::valid(&field.value))
            || saved
                .chatgpt_url
                .as_deref()
                .is_some_and(|url| validate_chatgpt_url(url).is_err())
        {
            return Err("Invalid live OpenAI context".into());
        }
        Ok(saved)
    }

    fn save_openai(&mut self, saved: &Saved) -> Result<(), String> {
        let bytes = serde_json::to_vec(saved).map_err(|_| "Cannot encode live OpenAI context")?;
        if bytes.len() > MAX_OBSERVATION_BYTES {
            return Err("Live OpenAI context exceeds its memory limit".into());
        }
        self.openai.insert(saved.id.clone(), saved.clone());
        Ok(())
    }

    fn observe_openai(
        &mut self,
        owner: &HistoryOwner,
        metadata: &OpenAiContext,
        observed_ms: u64,
    ) -> Result<bool, String> {
        if self.ownership.pending(owner)? {
            return Ok(false);
        }
        let mut saved = self.openai_state(owner.id())?;
        let session_changed = update(&mut saved.session, metadata.session.as_deref(), observed_ms);
        let subject_changed = update(&mut saved.subject, metadata.subject.as_deref(), observed_ms);
        if session_changed || subject_changed {
            self.save_openai(&saved)?;
        }
        Ok(true)
    }

    fn selected_openai(&self, owner: &HistoryOwner) -> Result<Option<Saved>, String> {
        let pending = self.ownership.pending(owner)?;
        if self.ownership.current.get().as_ref() != Some(owner) {
            return Err("The selected conversation changed; retry the command".into());
        }
        if pending {
            return Ok(None);
        }
        if self.active.as_ref().is_none_or(|active| {
            active.metadata.id != owner.id() || active.metadata.workspace != self.workspace
        }) {
            return Err("The selected conversation is no longer available".into());
        }
        self.openai_state(owner.id()).map(Some)
    }

    fn change_openai_url(
        &mut self,
        owner: &HistoryOwner,
        url: Option<&str>,
    ) -> Result<bool, String> {
        if let Some(url) = url {
            validate_chatgpt_url(url)?;
        }
        let mut saved = self
            .selected_openai(owner)?
            .ok_or("Start the current session before associating a ChatGPT URL")?;
        if saved.chatgpt_url.as_deref() == url {
            return Ok(false);
        }
        saved.chatgpt_url = url.map(str::to_owned);
        if saved.chatgpt_url.is_none() && saved.session.is_none() && saved.subject.is_none() {
            self.openai.remove(owner.id());
        } else {
            self.save_openai(&saved)?;
        }
        Ok(true)
    }
}

/// Record display metadata using the original verified live context capability.
pub(crate) fn observe(
    owner: &HistoryOwner,
    metadata: &OpenAiContext,
    observed_ms: u64,
) -> Result<bool, String> {
    with_owner(owner, |history, owner| {
        history.observe_openai(owner, metadata, observed_ms)
    })
}

/// Read current-process values only for the currently selected owner.
pub(crate) fn read_selected(owner: &HistoryOwner) -> Result<Option<Saved>, String> {
    with_owner(owner, |history, owner| history.selected_openai(owner))
}

/// Associate a validated URL with the original selected live context.
pub(crate) fn change_url(owner: &HistoryOwner, url: Option<&str>) -> Result<bool, String> {
    with_owner(owner, |history, owner| {
        history.change_openai_url(owner, url)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn memory() -> (PathBuf, History) {
        let path =
            std::env::temp_dir().join(super::super::random_id("ssmcp-openai-memory").unwrap());
        let history = History::with_workspace(path.clone(), "/openai-fixture".into()).unwrap();
        (path, history)
    }

    fn metadata(session: &str, subject: Option<&str>) -> OpenAiContext {
        OpenAiContext {
            session: Some(session.into()),
            subject: subject.map(str::to_owned),
        }
    }

    #[test]
    fn only_exact_https_chatgpt_conversation_paths_are_accepted() {
        for url in [
            "https://chatgpt.com/c/fictional-123",
            "https://chat.openai.com/c/fictional-id?model=fixture#message",
            "https://chatgpt.com:443/g/g-p-123/c/fictional_123",
        ] {
            assert!(validate_chatgpt_url(url).is_ok(), "{url}");
        }
        for url in [
            "http://chatgpt.com/c/fictional",
            "https://chatgpt.com.evil.test/c/fictional",
            "https://chatgpt.com@evil.test/c/fictional",
            "https://user:password@chatgpt.com/c/fictional",
            "https://@chatgpt.com/c/fictional",
            "https://chatgpt.com:444/c/fictional",
            "https://%63hatgpt.com/c/fictional",
            "https://chatgpt.com/../c/fictional",
            "https://chatgpt.com/c/%2F",
            "https://chatgpt.com/c/",
            "https://chatgpt.com/g//c/fictional",
            "https://chatgpt.com/share/fictional",
            "https://chatgpt.com/c/fictional/extra",
            "https://chatgpt.com/c/fictional\\other",
            "https://chatgpt.com/c/fictional\n",
            "javascript:document.domain",
        ] {
            assert!(validate_chatgpt_url(url).is_err(), "{url}");
        }
        assert!(
            validate_chatgpt_url(&format!("https://chatgpt.com/c/{}", "x".repeat(2048))).is_err()
        );
    }

    #[test]
    fn pending_observations_do_not_create_a_context_or_files() {
        let (path, mut history) = memory();
        let owner = history.capture_owner().unwrap();
        assert!(
            !history
                .observe_openai(&owner, &metadata("pending", None), 1)
                .unwrap()
        );
        assert!(history.selected_openai(&owner).unwrap().is_none());
        assert!(history.active.is_none());
        assert!(history.conversations.is_empty());
        assert!(history.openai.is_empty());
        assert!(!path.exists());
    }

    #[test]
    fn observations_are_bounded_to_the_original_ram_context_and_timestamp() {
        let (path, mut history) = memory();
        history.record("user", "Current task").unwrap();
        let owner = history.capture_owner().unwrap();
        assert!(
            history
                .observe_openai(&owner, &metadata("newer", None), 20)
                .unwrap()
        );
        assert!(
            history
                .observe_openai(&owner, &metadata("older", Some("subject")), 10)
                .unwrap()
        );
        let saved = history.selected_openai(&owner).unwrap().unwrap();
        assert_eq!(saved.session.unwrap().value, "newer");
        assert_eq!(saved.subject.unwrap().value, "subject");
        assert!(!path.exists());

        let mut other = History::with_workspace(path.clone(), "/openai-fixture".into()).unwrap();
        assert!(other.openai.is_empty());
        assert!(
            other
                .observe_openai(&owner, &metadata("wrong process", None), 30)
                .is_err()
        );
        assert!(other.selected_openai(&owner).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn selected_reads_and_url_changes_reject_a_different_owner() {
        let (path, mut history) = memory();
        history.record("user", "Current task").unwrap();
        let owner = history.capture_owner().unwrap();
        history
            .observe_openai(&owner, &metadata("original", None), 1)
            .unwrap();
        history
            .ownership
            .select(&super::super::random_id("conversation").unwrap(), true);
        assert!(history.selected_openai(&owner).is_err());
        assert!(
            history
                .change_openai_url(&owner, Some("https://chatgpt.com/c/other"))
                .is_err()
        );
        assert!(
            history
                .observe_openai(&owner, &metadata("late-original", None), 2)
                .unwrap()
        );
        assert_eq!(
            history
                .openai_state(owner.id())
                .unwrap()
                .session
                .unwrap()
                .value,
            "late-original"
        );
        assert!(!path.exists());
    }

    #[test]
    fn revoked_owner_cannot_read_write_or_recreate_observations() {
        let (path, mut history) = memory();
        history.record("user", "Current task").unwrap();
        let owner = history.capture_owner().unwrap();
        history
            .observe_openai(&owner, &metadata("original", None), 1)
            .unwrap();
        history.ownership.revoke(owner.id());
        assert!(
            history
                .observe_openai(&owner, &metadata("stale", None), 2)
                .is_err()
        );
        assert!(history.selected_openai(&owner).is_err());
        assert!(
            history
                .change_openai_url(&owner, Some("https://chatgpt.com/c/stale"))
                .is_err()
        );
        assert_eq!(
            history
                .openai_state(owner.id())
                .unwrap()
                .session
                .unwrap()
                .value,
            "original"
        );
        assert!(!path.exists());
    }

    #[test]
    fn url_association_is_validated_idempotent_and_memory_only() {
        let (path, mut history) = memory();
        history.record("user", "Current task").unwrap();
        let owner = history.capture_owner().unwrap();
        let url = "https://chatgpt.com/c/live-session";
        assert!(history.change_openai_url(&owner, Some(url)).unwrap());
        assert!(!history.change_openai_url(&owner, Some(url)).unwrap());
        assert!(
            history
                .change_openai_url(&owner, Some("https://evil.test/c/session"))
                .is_err()
        );
        assert_eq!(
            history
                .selected_openai(&owner)
                .unwrap()
                .unwrap()
                .chatgpt_url
                .as_deref(),
            Some(url)
        );
        assert!(history.change_openai_url(&owner, None).unwrap());
        assert!(history.openai.is_empty());
        assert!(!history.change_openai_url(&owner, None).unwrap());
        assert!(!path.exists());
    }
}
