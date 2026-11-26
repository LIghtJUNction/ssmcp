use serde::{Deserialize, Serialize};

/// A comparison of committed objects from the client's local remote-tracking ref.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitTrackingChange {
    pub branch: String,
    pub upstream: String,
    pub head: String,
    pub upstream_oid: String,
    pub previous_upstream_oid: String,
    pub ahead: u64,
    pub behind: u64,
}

impl GitTrackingChange {
    pub(crate) fn valid(&self) -> bool {
        let valid_ref = |name: &str| {
            !name.is_empty()
                && name.len() <= 512
                && !name.contains("..")
                && !name.contains("@{")
                && !name.contains("//")
                && !name.ends_with('.')
                && name.split('/').all(|part| {
                    !part.is_empty() && !part.starts_with('.') && !part.ends_with(".lock")
                })
                && !name.chars().any(|ch| {
                    ch.is_control()
                        || ch.is_whitespace()
                        || matches!(ch, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
                })
                && crate::audit::event(serde_json::json!(name)).as_str() == Some(name)
        };
        let oid = |s: &str| matches!(s.len(), 40 | 64) && s.bytes().all(|c| c.is_ascii_hexdigit());
        valid_ref(&self.branch)
            && self
                .upstream
                .strip_prefix("refs/remotes/")
                .is_some_and(valid_ref)
            && oid(&self.head)
            && oid(&self.upstream_oid)
            && oid(&self.previous_upstream_oid)
            && self.head.len() == self.upstream_oid.len()
            && self.head.len() == self.previous_upstream_oid.len()
    }
}
