//! Display-only, bounded authentication diagnostics. Native events and archives
//! retain their own ordering/content; this cache only updates local warning cards.
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

const MAX_GROUPS: usize = 64;
const MAX_PREVIEW: usize = 2048;

#[derive(Clone, Debug)]
pub(crate) struct Update {
    pub id: u64,
    pub source: String,
    pub summary: String,
    pub message: String,
    pub preview: String,
    pub occurrences: u64,
}

impl Update {
    pub(crate) fn heading(&self) -> String {
        format!(
            "[WARN] {}: {} · ×{}",
            self.source, self.summary, self.occurrences
        )
    }

    pub(crate) fn text(&self) -> String {
        format!(
            "{}\n{}\nRepeated occurrences update this record.\n\nDiagnostic preview:\n{}\n",
            self.heading(),
            self.message,
            self.preview
        )
    }
}

struct Entry {
    update: Update,
    backend_key: [u8; 32],
    updated: Instant,
    reported: Instant,
    reported_count: u64,
    closed: bool,
}

#[derive(Default)]
pub(super) struct AuthDiagnostics {
    entries: HashMap<[u8; 32], Entry>,
    updates: Vec<Update>,
    next_id: u64,
}

impl AuthDiagnostics {
    pub(super) fn observe(&mut self, source: &str, text: &str, now: Instant) -> Option<Update> {
        if !text.contains("AuthRequired") && !text.contains("auth_required") {
            return None;
        }
        let (canonical, origins) = canonical(text);
        let mut digest = Sha256::new();
        digest.update((source.len() as u64).to_be_bytes());
        digest.update(source.as_bytes());
        digest.update(canonical.as_bytes());
        let key: [u8; 32] = digest.finalize().into();
        let first = !self.entries.contains_key(&key);
        if first {
            if self.entries.len() >= MAX_GROUPS
                && !self.entries.contains_key(&key)
                && let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.updated)
                    .map(|(key, _)| *key)
            {
                self.entries.remove(&oldest);
            }
            let id = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            let origin = if origins.is_empty() {
                String::new()
            } else {
                format!(" ({})", origins.join(", "))
            };
            self.entries.insert(key, Entry {
                backend_key: Sha256::digest(source.as_bytes()).into(),
                update: Update {
                    id,
                    source: super::activity::single_line(&super::clean(source), 120),
                    summary: format!("MCP sign-in required{origin}"),
                    message: format!("Backend MCP authentication required{origin}.\nSign in to that MCP server in this backend, or disable its connector.\nOriginal diagnostics: --verbose or --raw-events; backend event archive."),
                    preview: preview(&canonical),
                    occurrences: 0,
                },
                updated: now,
                reported: now,
                reported_count: 1,
                closed: false,
            });
        }
        let entry = self
            .entries
            .get_mut(&key)
            .expect("diagnostic just inserted");
        entry.update.occurrences = entry.update.occurrences.saturating_add(1);
        entry.updated = now;
        entry.closed = false;
        let update = entry.update.clone();
        if let Some(previous) = self.updates.iter_mut().find(|old| old.id == update.id) {
            *previous = update.clone();
        } else {
            if self.updates.len() >= MAX_GROUPS {
                self.updates.remove(0);
            }
            self.updates.push(update.clone());
        }
        Some(update)
    }

    pub(super) fn take_updates(&mut self) -> Vec<Update> {
        std::mem::take(&mut self.updates)
    }

    pub(super) fn reports(
        &mut self,
        source: Option<&str>,
        now: Instant,
        force: bool,
    ) -> Vec<(String, String)> {
        self.entries.values_mut().filter_map(|entry| {
            if source.is_some_and(|source| entry.backend_key != <[u8; 32]>::from(Sha256::digest(source.as_bytes())))
                || entry.update.occurrences <= entry.reported_count
                || (!force && now.saturating_duration_since(entry.reported) < Duration::from_secs(30))
            {
                return None;
            }
            let repeats = entry.update.occurrences.saturating_sub(entry.reported_count);
            entry.reported_count = entry.update.occurrences;
            entry.reported = now;
            Some((entry.update.source.clone(), format!(
                "same diagnostic ×{} · {repeats} repeats folded · {} · --verbose for full text",
                entry.update.occurrences, entry.update.summary
            )))
        }).collect()
    }

    pub(super) fn close(&mut self, source: &str) {
        let backend_key: [u8; 32] = Sha256::digest(source.as_bytes()).into();
        for entry in self
            .entries
            .values_mut()
            .filter(|entry| entry.backend_key == backend_key)
        {
            entry.closed = true;
        }
    }

    pub(super) fn snapshot(&self, now: Instant) -> Vec<Value> {
        let mut entries: Vec<_> = self.entries.values().collect();
        entries.sort_by_key(|entry| entry.update.id);
        entries.into_iter().map(|entry| json!({
            "source":entry.update.source,
            "occurrences":entry.update.occurrences,
            "folded":entry.update.occurrences.saturating_sub(1),
            "message":entry.update.message,
            "closed":entry.closed,
            "age_ms":now.saturating_duration_since(entry.updated).as_millis().min(u128::from(u64::MAX)) as u64
        })).collect()
    }
}

fn preview(text: &str) -> String {
    if text.len() <= MAX_PREVIEW {
        return text.into();
    }
    let mut end = MAX_PREVIEW;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}… [preview truncated; inspect original event archive]",
        &text[..end]
    )
}

fn url_start(text: &str) -> Option<usize> {
    [b"https://".as_slice(), b"http://".as_slice()]
        .into_iter()
        .filter_map(|scheme| {
            text.as_bytes()
                .windows(scheme.len())
                .position(|part| part.eq_ignore_ascii_case(scheme))
        })
        .min()
}

/// The diagnostic copy keeps URL paths and the audit policy's safe query fields,
/// but must also mask userinfo containing quotes/parentheses before an '@'.
pub(super) fn redact_url_userinfo(text: &str) -> String {
    let mut result = String::new();
    let mut remaining = text;
    while let Some(start) = url_start(remaining) {
        let authority = start + remaining[start..].find("://").expect("matched URL scheme") + 3;
        let end = remaining[authority..]
            .find(|ch: char| ch.is_whitespace() || matches!(ch, '/' | '?' | '#'))
            .map_or(remaining.len(), |offset| authority + offset);
        result.push_str(&remaining[..authority]);
        if let Some(at) = remaining[authority..end].rfind('@') {
            result.push_str("[redacted]");
            result.push_str(&remaining[authority + at..end]);
        } else {
            result.push_str(&remaining[authority..end]);
        }
        remaining = &remaining[end..];
    }
    result.push_str(remaining);
    result
}

/// Preserve every non-URL diagnostic detail. URLs are compared and displayed by
/// their safe origin only, never by credentials, paths, queries, or fragments.
fn canonical(text: &str) -> (String, Vec<String>) {
    let mut result = String::new();
    let mut origins = Vec::new();
    let mut remaining = text;
    while let Some(start) = url_start(remaining) {
        result.push_str(&remaining[..start]);
        let url_text = &remaining[start..];
        // Quotes and parentheses are valid inside URL userinfo. Parse the whole
        // token first; trim only trailing log/JSON wrappers, never at a password.
        let end = url_text.find(char::is_whitespace).unwrap_or(url_text.len());
        let token = &url_text[..end];
        let candidate = token.trim_end_matches(['"', '\'', '\\', ')', '}', '>', ',', ';']);
        let origin = reqwest::Url::parse(candidate)
            .ok()
            .filter(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
            .map(|url| url.origin().ascii_serialization());
        if let Some(origin) = origin {
            result.push_str(&origin);
            let displayed = if origin.len() > 256 {
                format!("{}…", &origin[..256])
            } else {
                origin.clone()
            };
            if !origins.contains(&displayed) && origins.len() < 4 {
                origins.push(displayed);
            }
            result.push_str(&token[candidate.len()..]);
        } else {
            // Keep malformed URLs distinct without displaying their possible secrets.
            result.push_str(&format!(
                "[invalid URL {:x}]",
                crate::digest::Hex(Sha256::digest(candidate.as_bytes()))
            ));
        }
        remaining = &url_text[end..];
    }
    result.push_str(remaining);
    (result, origins)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_origins_update_only_their_own_bounded_group() {
        let mut diagnostics = AuthDiagnostics::default();
        let now = Instant::now();
        let a = "ERROR rmcp AuthRequired https://api.x.com/.well-known/oauth-protected-resource";
        let b = "ERROR rmcp AuthRequired https://api.lmm.best/.well-known/oauth-protected-resource";
        let updates = [a, b, a, b].map(|line| diagnostics.observe("codex", line, now).unwrap());
        assert_eq!(updates[0].id, updates[2].id);
        assert_eq!(updates[1].id, updates[3].id);
        assert_ne!(updates[0].id, updates[1].id);
        assert_eq!(updates[2].occurrences, 2);
        assert_eq!(updates[3].occurrences, 2);
        assert_eq!(diagnostics.take_updates().len(), 2);
    }

    #[test]
    fn origin_normalization_redacts_urls_without_merging_other_errors_or_backends() {
        let mut diagnostics = AuthDiagnostics::default();
        let now = Instant::now();
        let first = diagnostics.observe("codex", "ERROR AuthRequired https://alice:fixture-secret@API.X.COM:443/a?token=secret#private", now).unwrap();
        let same = diagnostics
            .observe(
                "codex",
                "ERROR AuthRequired https://api.x.com/b?token=changed",
                now,
            )
            .unwrap();
        assert_eq!(first.id, same.id);
        let changed = diagnostics
            .observe(
                "codex",
                "ERROR different AuthRequired https://api.x.com/b",
                now,
            )
            .unwrap();
        let backend = diagnostics
            .observe("pi", "ERROR AuthRequired https://api.x.com/b", now)
            .unwrap();
        assert_ne!(changed.id, same.id);
        assert_ne!(backend.id, same.id);
        assert!(
            diagnostics
                .observe("codex", "ERROR transport failed https://api.x.com", now)
                .is_none()
        );
        let display = same.text();
        for secret in ["alice", "fixture-secret", "?token", "#private", "/b"] {
            assert!(!display.contains(secret), "{display}");
        }
        assert!(display.contains("https://api.x.com"));
    }

    #[test]
    fn short_backend_restarts_reuse_display_groups_but_process_reset_starts_fresh() {
        let mut diagnostics = AuthDiagnostics::default();
        let now = Instant::now();
        let line = "AuthRequired https://api.x.com";
        let old = diagnostics.observe("codex", line, now).unwrap();
        diagnostics.observe("codex", line, now).unwrap();
        diagnostics.close("codex");
        let fresh = diagnostics.observe("codex", line, now).unwrap();
        assert_eq!(old.id, fresh.id);
        assert_eq!(fresh.occurrences, 3);
        let reset = AuthDiagnostics::default()
            .observe("codex", line, now)
            .unwrap();
        assert_eq!(reset.occurrences, 1);
    }

    #[test]
    fn state_and_pending_updates_stay_bounded_and_bad_urls_do_not_echo_secrets() {
        let mut diagnostics = AuthDiagnostics::default();
        let now = Instant::now();
        for index in 0..MAX_GROUPS * 3 {
            diagnostics
                .observe(
                    "codex",
                    &format!("AuthRequired error-{index} https://api.x.com"),
                    now + Duration::from_millis(index as u64),
                )
                .unwrap();
        }
        assert_eq!(diagnostics.entries.len(), MAX_GROUPS);
        assert_eq!(diagnostics.updates.len(), MAX_GROUPS);
        let first = diagnostics
            .observe("codex", "AuthRequired https://[malformed-secret", now)
            .unwrap();
        let other = diagnostics
            .observe("codex", "AuthRequired https://[different-secret", now)
            .unwrap();
        assert_ne!(first.id, other.id);
        assert!(!first.text().contains("malformed-secret"));
    }

    #[test]
    fn display_names_do_not_change_backend_close_identity() {
        let mut diagnostics = AuthDiagnostics::default();
        let source = format!("codex {}", "界".repeat(200));
        let now = Instant::now();
        let first = diagnostics
            .observe(&source, "AuthRequired https://api.x.com", now)
            .unwrap();
        diagnostics
            .observe(&source, "AuthRequired https://api.x.com", now)
            .unwrap();
        assert_eq!(diagnostics.reports(Some(&source), now, true).len(), 1);
        diagnostics.close(&source);
        assert!(diagnostics.entries.values().all(|entry| entry.closed));
        let next = diagnostics
            .observe(&source, "AuthRequired https://api.x.com", now)
            .unwrap();
        assert_eq!(first.id, next.id);
        assert_eq!(next.occurrences, 3);
        assert!(diagnostics.entries.values().all(|entry| !entry.closed));
    }

    #[test]
    fn upper_case_and_punctuation_in_credentials_never_reach_display() {
        let now = Instant::now();
        for credential in [
            "alice:fixture'pw",
            "alice:fixture)pw",
            "alice:fixture-secret",
        ] {
            let mut diagnostics = AuthDiagnostics::default();
            let first = diagnostics
                .observe(
                    "codex",
                    &format!("AuthRequired HTTPS://{credential}@API.X.COM:443/a?secret=yes"),
                    now,
                )
                .unwrap();
            let other = diagnostics
                .observe("codex", "AuthRequired https://api.x.com/other", now)
                .unwrap();
            assert_eq!(first.id, other.id);
            for secret in ["alice", "fixture", "secret=yes"] {
                assert!(!first.text().contains(secret), "{}", first.text());
            }
            assert!(first.text().contains("https://api.x.com"));
        }
    }

    #[test]
    fn long_origins_and_unicode_details_keep_each_display_update_bounded() {
        let mut diagnostics = AuthDiagnostics::default();
        let host = format!("{}.example", "a".repeat(16 * 1024));
        let line = format!(
            "AuthRequired https://{host}/private {}",
            "界".repeat(32 * 1024)
        );
        let update = diagnostics.observe("codex", &line, Instant::now()).unwrap();
        assert!(update.text().len() < 8 * 1024, "{}", update.text().len());
        assert!(update.preview.len() < MAX_PREVIEW + 100);
        assert!(update.message.len() < 2048);
    }

    #[test]
    fn raw_window_userinfo_masking_preserves_safe_url_path_and_each_record() {
        for text in [
            "AuthRequired HTTPS://alice:fixture'pw@api.x.com/resource\n",
            "AuthRequired https://alice:fixture)pw@api.x.com/resource\n",
        ] {
            let safe = redact_url_userinfo(text);
            assert!(
                !safe.contains("alice") && !safe.contains("fixture"),
                "{safe}"
            );
            assert!(safe.contains("[redacted]@api.x.com/resource"), "{safe}");
            assert!(safe.ends_with('\n'));
            assert_eq!(redact_url_userinfo(&safe), safe);
        }
    }
}
