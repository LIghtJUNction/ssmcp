//! Connection-local observations from the client's configured Git workspace.
use serde::{Deserialize, Serialize};
#[cfg(any(target_os = "linux", test))]
use serde_json::{Value, json};

pub const CAPABILITY: &str = "repo_events_v1";
pub(crate) const UPSTREAM_CHANGED: &str = "repo.upstream_changed";
pub(crate) const PULL_REQUEST_OPENED: &str = "repo.pull_request_opened";
pub(crate) const MAX_NUMBER: u64 = (1 << 53) - 1;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Counts {
    pub ahead: u64,
    pub behind: u64,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Event {
    UpstreamChanged {
        branch: String,
        upstream: String,
        ahead: u64,
        behind: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous: Option<Counts>,
    },
    PullRequestOpened {
        remote: String,
        number: u64,
    },
}

pub(crate) fn valid_label(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value.starts_with('-')
        && !value.contains(|c: char| c.is_control() || c.is_whitespace() || c == ':')
}

impl Event {
    pub fn validate(&self) -> Result<(), String> {
        let valid = match self {
            Self::UpstreamChanged {
                branch,
                upstream,
                ahead,
                behind,
                previous,
            } => {
                valid_label(branch)
                    && valid_label(upstream)
                    && *ahead <= MAX_NUMBER
                    && *behind <= MAX_NUMBER
                    && previous
                        .is_none_or(|prior| prior.ahead <= MAX_NUMBER && prior.behind <= MAX_NUMBER)
            }
            Self::PullRequestOpened { remote, number } => {
                valid_label(remote) && (1..=MAX_NUMBER).contains(number)
            }
        };
        if valid {
            Ok(())
        } else {
            Err("invalid repository event metadata".into())
        }
    }
}

#[cfg(any(target_os = "linux", test))]
impl Event {
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::UpstreamChanged { .. } => UPSTREAM_CHANGED,
            Self::PullRequestOpened { .. } => PULL_REQUEST_OPENED,
        }
    }

    pub(crate) fn into_data(self) -> Value {
        match self {
            Self::UpstreamChanged {
                branch,
                upstream,
                ahead,
                behind,
                previous,
            } => {
                json!({ "branch": branch, "upstream": upstream, "ahead": ahead, "behind": behind, "previous": previous })
            }
            Self::PullRequestOpened { remote, number } => {
                json!({ "remote": remote, "number": number })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wire::Frame;

    #[test]
    fn repository_metadata_rejects_forged_scope_and_invalid_values() {
        for data in [
            json!({"kind":"pull_request_opened","remote":"origin","number":0}),
            json!({"kind":"pull_request_opened","remote":"origin","number":1,"client_id":"other"}),
            json!({"kind":"pull_request_opened","remote":"https://example.test","number":1}),
            json!({"kind":"pull_request_opened","remote":"x".repeat(1025),"number":1}),
            json!({"kind":"worker_completed","number":3}),
            json!({"kind":"upstream_changed","branch":"main","upstream":"origin/main","ahead":0,"behind":1,"upstream_oid":"a".repeat(40)}),
            json!({"kind":"upstream_changed","branch":"main\n","upstream":"origin/main","ahead":0,"behind":1}),
            json!({"kind":"upstream_changed","branch":"main","upstream":"origin/main","ahead":0,"behind":MAX_NUMBER+1}),
            json!({"kind":"upstream_changed","branch":"main","upstream":"origin/main","ahead":0,"behind":0,"previous":{"ahead":0,"behind":1,"token":"foreign"}}),
        ] {
            assert!(serde_json::from_value::<Event>(data).map_or(true, |e| e.validate().is_err()));
        }
        let offered = Frame::RepositoryEvent {
            event: Event::UpstreamChanged {
                branch: "feature/repo".into(),
                upstream: "upstream/trunk".into(),
                ahead: 2,
                behind: 7,
                previous: Some(Counts {
                    ahead: 1,
                    behind: 1,
                }),
            },
        };
        let frame = serde_json::to_value(offered).unwrap();
        let Frame::RepositoryEvent { event } = serde_json::from_value(frame).unwrap() else {
            panic!("expected repository event");
        };
        assert_eq!(event.validate(), Ok(()));
        assert_eq!(event.kind(), UPSTREAM_CHANGED);
        assert_eq!(event.into_data()["previous"]["behind"], 1);
    }
}
