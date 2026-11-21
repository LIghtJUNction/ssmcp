//! Private caller observations associated with a saved local conversation.
use super::{History, HistoryOwner, ownership::with_owner, session_name};
use crate::wire::openai::{self, OpenAiContext};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use std::io::Read as _;

const MAX_SIDECAR_BYTES: usize = 16 * 1024;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Observed {
    pub(crate) value: String,
    pub(crate) observed_ms: u64,
}

#[derive(Deserialize, Serialize)]
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

fn name(id: &str) -> Result<String, String> {
    session_name(id)?;
    Ok(format!("{id}.openai.json"))
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
    fn openai_sidecar(&self, id: &str) -> Result<Saved, String> {
        let filename = name(id)?;
        if !self.directory.names()?.contains(&filename) {
            return Ok(Saved {
                schema_version: 1,
                id: id.into(),
                workspace: self.workspace.clone(),
                session: None,
                subject: None,
                chatgpt_url: None,
            });
        }
        let mut bytes = Vec::new();
        self.directory
            .open(&filename, false, false, false)?
            .take(MAX_SIDECAR_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| "Cannot read saved OpenAI context")?;
        if bytes.len() > MAX_SIDECAR_BYTES {
            return Err("Saved OpenAI context exceeds its storage limit".into());
        }
        let saved: Saved =
            serde_json::from_slice(&bytes).map_err(|_| "Invalid saved OpenAI context")?;
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
            return Err("Invalid saved OpenAI context".into());
        }
        Ok(saved)
    }

    fn save_openai(&self, saved: &Saved) -> Result<(), String> {
        let bytes = serde_json::to_vec(saved).map_err(|_| "Cannot encode saved OpenAI context")?;
        if bytes.len() > MAX_SIDECAR_BYTES {
            return Err("Saved OpenAI context exceeds its storage limit".into());
        }
        self.directory.write_atomic(&name(&saved.id)?, &bytes)
    }

    fn observe_openai(
        &mut self,
        owner: &HistoryOwner,
        metadata: &OpenAiContext,
        observed_ms: u64,
    ) -> Result<bool, String> {
        let _lock = self.directory.lock()?;
        if self.ownership.pending(owner)? {
            return Ok(false);
        }
        let id = owner.id();
        if self.archive_index()?.contains(id) {
            return Err("The original conversation is archived".into());
        }
        // Keep the original lease when current; a switch neither transfers
        // ownership nor authorizes writing through another process's lease.
        let _lease = match &self.active {
            Some(active)
                if self.ownership.current.get().as_ref() == Some(owner)
                    && active.metadata.id == id
                    && active.metadata.workspace == self.workspace =>
            {
                None
            }
            _ => {
                let lease = self.lease(id)?;
                if self.load(id)?.metadata.workspace != self.workspace {
                    return Err("The original conversation belongs to a different workspace".into());
                }
                Some(lease)
            }
        };
        let mut saved = self.openai_sidecar(id)?;
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
        if self.archive_index()?.contains(owner.id())
            || self.active.as_ref().is_none_or(|active| {
                active.metadata.id != owner.id() || active.metadata.workspace != self.workspace
            })
        {
            return Err("The selected conversation is no longer available".into());
        }
        self.openai_sidecar(owner.id()).map(Some)
    }

    pub(super) fn remove_openai(&self, id: &str, names: &[String]) -> Result<(), String> {
        let filename = name(id)?;
        if names.contains(&filename) {
            self.directory.remove(&filename)?;
        }
        Ok(())
    }
}

/// Record display metadata using the original verified history capability.
pub(crate) fn observe(
    owner: &HistoryOwner,
    metadata: &OpenAiContext,
    observed_ms: u64,
) -> Result<bool, String> {
    with_owner(owner, |history, owner| {
        history.observe_openai(owner, metadata, observed_ms)
    })
}

/// Read the saved values only through the currently selected history lease.
pub(crate) fn read_selected(owner: &HistoryOwner) -> Result<Option<Saved>, String> {
    with_owner(owner, |history, owner| {
        let _lock = history.directory.lock()?;
        history.selected_openai(owner)
    })
}

/// Update only the explicitly supplied URL on the original selected history.
pub(crate) fn change_url(owner: &HistoryOwner, url: Option<&str>) -> Result<bool, String> {
    if let Some(url) = url {
        validate_chatgpt_url(url)?;
    }
    with_owner(owner, |history, owner| {
        let _lock = history.directory.lock()?;
        let mut saved = history
            .selected_openai(owner)?
            .ok_or("Start or resume a saved conversation before associating a ChatGPT URL")?;
        if saved.chatgpt_url.as_deref() == url {
            return Ok(false);
        }
        saved.chatgpt_url = url.map(str::to_owned);
        if saved.chatgpt_url.is_none() && saved.session.is_none() && saved.subject.is_none() {
            history.remove_openai(owner.id(), &history.directory.names()?)?;
        } else {
            history.save_openai(&saved)?;
        }
        Ok(true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        runtime::call_scope::CallScope,
        wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution},
    };
    use std::sync::{Arc, Mutex};

    fn scope(thread: Option<char>, request: u64) -> Arc<CallScope> {
        let client = "a".repeat(64);
        let agent = thread.map(|digit| ControlAgent {
            id: format!("agent_{}", digit.to_string().repeat(32)),
            name: "fixture agent".into(),
        });
        let context = CodeContext {
            run_id: "b".repeat(32),
            code: "return null".into(),
            timestamp_ms: request,
            session_id: agent.as_ref().map(|_| client.clone()),
            thread_id: agent.as_ref().map(|agent| agent.id.clone()),
        };
        let attribution = ControlAttribution {
            account: ControlAccount {
                provider: "github".into(),
                subject: "123".into(),
                login: "fixture-account".into(),
            },
            owner: None,
            agent,
        };
        CallScope::from_relay_for_client(
            1,
            Some(&client),
            request,
            Some(&attribution),
            Some(&context),
        )
        .unwrap()
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
    fn retention_reclaims_sidecar_with_its_closed_history() {
        let temp = super::super::tests::Temporary::new();
        let mut history = temp.history();
        history.record("user", "Saved for fixture").unwrap();
        let owner = history.capture_owner().unwrap();
        assert!(
            history
                .observe_openai(&owner, &metadata("synthetic", None), 11)
                .unwrap()
        );
        let sidecar = name(owner.id()).unwrap();
        assert!(history.directory.names().unwrap().contains(&sidecar));
        history.release_current();

        let replacement = super::super::random_id("conversation").unwrap();
        history
            .retain(&replacement, super::super::MAX_TOTAL_BYTES)
            .unwrap();
        let names = history.directory.names().unwrap();
        assert!(!names.contains(&sidecar));
        assert!(!names.contains(&session_name(owner.id()).unwrap()));
        assert!(
            history
                .observe_openai(&owner, &metadata("stale", None), 12)
                .is_err()
        );
    }

    #[tokio::test]
    async fn late_openai_observation_cannot_relabel_another_history_or_replace_newer_values() {
        use super::super::{HistoryStore, ownership::TEST_HISTORY, tests::Temporary, with};
        let temp = Temporary::new();
        let mut history = temp.history();
        history.record("user", "Conversation B").unwrap();
        let b = history.capture_owner().unwrap();
        history.release_current();
        history.record("user", "Conversation A").unwrap();
        let a = history.capture_owner().unwrap();
        let store = HistoryStore {
            current: history.ownership.current.clone(),
            state: Mutex::new(Some(history)),
        };
        TEST_HISTORY
            .scope(Arc::new(store), async {
                let a_call = scope(Some('1'), 1);
                let a_late = a_call
                    .openai_observation(metadata("session-A", Some("claimed-B")), 10)
                    .unwrap();
                let a_newer = scope(Some('1'), 2)
                    .openai_observation(metadata("session-A-newer", None), 20)
                    .unwrap();
                assert!(a_newer.record().unwrap());

                with(|history| history.resume(b.id())).unwrap();
                let unbound = scope(None, 3);
                assert!(
                    unbound
                        .openai_observation(metadata("unbound", Some("unbound")), 30)
                        .is_none()
                );
                let b_call = scope(Some('2'), 4);
                let b_write = b_call
                    .openai_observation(metadata("session-B", Some("subject-B")), 40)
                    .unwrap();
                assert!(b_write.record().unwrap());
                assert!(a_late.record().unwrap());
                assert_eq!(a_call.attribution().unwrap().account.subject, "123");
                assert_eq!(b_call.attribution().unwrap().account.subject, "123");

                with(|history| {
                    let saved_a = history.openai_sidecar(a.id())?;
                    let saved_b = history.openai_sidecar(b.id())?;
                    assert_eq!(saved_a.session.unwrap().value, "session-A-newer");
                    assert_eq!(saved_a.subject.unwrap().value, "claimed-B");
                    assert_eq!(saved_b.session.unwrap().value, "session-B");
                    assert_eq!(saved_b.subject.unwrap().value, "subject-B");
                    assert_eq!(history.active.as_ref().unwrap().metadata.id, b.id());
                    // The old verified thread remains pinned and cannot write
                    // through its revoked lease after archive.
                    history.archive_change(Some(a.id()), true)?;
                    Ok(())
                })
                .unwrap();
                assert!(a_late.record().is_err());
            })
            .await;
    }
}
