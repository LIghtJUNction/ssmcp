//! Per-connection access changes. Only the authenticated device stream calls this module.
use super::Session;
use crate::{
    identity::{Identity, IdentityProviders},
    wire::{
        AccessCommand, AccessEntry, AccessSnapshot, MAX_ALLOWED_PRINCIPALS,
        canonical_github_principal,
    },
};

fn same_identity(left: &Identity, right: &Identity) -> bool {
    left.provider == right.provider && left.subject == right.subject
}

fn snapshot(allow: &[Identity], owner: &Identity, message: Option<String>) -> AccessSnapshot {
    let mut entries: Vec<_> = allow
        .iter()
        .map(|identity| AccessEntry {
            principal: format!("{}@github", identity.login.to_ascii_lowercase()),
            login: identity.login.clone(),
            subject: identity.subject.clone(),
            owner: same_identity(identity, owner),
        })
        .collect();
    entries.sort_by(|left, right| {
        right
            .owner
            .cmp(&left.owner)
            .then_with(|| left.principal.cmp(&right.principal))
    });
    AccessSnapshot { entries, message }
}

impl Session {
    pub(super) async fn access_command(
        &self,
        providers: &IdentityProviders,
        owner: &Identity,
        command: AccessCommand,
    ) -> Result<AccessSnapshot, String> {
        if !self.is_alive()
            || owner.provider != "github"
            || self.owner_subject.as_deref() != Some(owner.subject.as_str())
            || !crate::management::valid_identity(owner)
            || !self
                .allowed()
                .iter()
                .any(|identity| same_identity(identity, owner))
        {
            return Err("当前连接没有可验证的会话所有者".into());
        }
        match command {
            AccessCommand::List {} => Ok(snapshot(&self.allowed(), owner, None)),
            AccessCommand::Add { principal } => {
                let principal = canonical_github_principal(&principal)?;
                // A login is a lookup key. Only GitHub's verified numeric subject grants access.
                let identity = providers
                    .resolve_allow(std::slice::from_ref(&principal))
                    .await?
                    .into_iter()
                    .next()
                    .ok_or("GitHub 用户不存在")?;
                if !self.is_alive() {
                    return Err("会话连接已关闭，权限未更改".into());
                }
                let mut allow = self.allowed();
                if allow
                    .iter()
                    .any(|allowed| same_identity(allowed, &identity))
                {
                    return Ok(snapshot(
                        &allow,
                        owner,
                        Some("该 GitHub 账号已在允许清单中".into()),
                    ));
                }
                if allow
                    .iter()
                    .any(|allowed| allowed.login.eq_ignore_ascii_case(&identity.login))
                {
                    return Err(
                        "该用户名对应的账号已变化；请先撤销清单中的旧条目，再重新添加".into(),
                    );
                }
                if allow.len() >= MAX_ALLOWED_PRINCIPALS {
                    return Err(format!(
                        "允许清单最多 {MAX_ALLOWED_PRINCIPALS} 个账号（包含所有者）"
                    ));
                }
                allow.push(identity);
                Ok(snapshot(
                    &allow,
                    owner,
                    Some(format!("已允许 {principal} 控制当前会话")),
                ))
            }
            AccessCommand::Remove { principal } => {
                let principal = canonical_github_principal(&principal)?;
                let mut allow = self.allowed();
                // Select an existing grant's stored identity. A rename or a GitHub outage
                // must not prevent revocation or transfer a grant to a recycled login.
                let Some(index) = allow.iter().position(|identity| {
                    format!("{}@github", identity.login.to_ascii_lowercase()) == principal
                }) else {
                    return Err("该 GitHub 用户不在当前会话的允许清单中".into());
                };
                if same_identity(&allow[index], owner) {
                    return Err("不能撤销当前会话的所有者".into());
                }
                allow.remove(index);
                self.steering.wake_waiters();
                Ok(snapshot(
                    &allow,
                    owner,
                    Some(format!("已撤销 {principal} 对当前会话的控制权限")),
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::broker::{Authorization, Broker, IDENTITY_RESOURCE};
    use crate::wire::{Controller, ControllerSource, Operation};
    use std::sync::Arc;

    fn identity(subject: &str, login: &str) -> Identity {
        Identity {
            provider: "github".into(),
            subject: subject.into(),
            login: login.into(),
        }
    }

    async fn session(broker: &Broker, login_code: &str) -> Arc<Session> {
        let mut session = broker.test_register("gh-42", login_code).await;
        broker
            .sessions
            .write()
            .await
            .get_mut("gh-42")
            .unwrap()
            .remove(session.client_id());
        Arc::get_mut(&mut session).unwrap().owner_subject = Some("42".into());
        session.allowed().push(identity("42", "alice"));
        broker
            .sessions
            .write()
            .await
            .entry("gh-42".into())
            .or_default()
            .insert(session.client_id().into(), session.clone());
        session
    }

    async fn provider() -> (IdentityProviders, tokio::task::JoinHandle<()>) {
        async fn lookup(
            axum::extract::Path(login): axum::extract::Path<String>,
        ) -> (axum::http::StatusCode, axum::Json<serde_json::Value>) {
            let id = match login.as_str() {
                "alice" => 42,
                "bob" => 7,
                "imposter" => 999,
                _ => login
                    .strip_prefix('u')
                    .and_then(|id| id.parse::<u64>().ok())
                    .map(|id| id + 100)
                    .unwrap_or(0),
            };
            (
                if id == 0 {
                    axum::http::StatusCode::NOT_FOUND
                } else {
                    axum::http::StatusCode::OK
                },
                axum::Json(
                    serde_json::json!({"id":id,"login":if login == "imposter" {"bob"} else {&login},"type":"User"}),
                ),
            )
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            axum::serve(
                listener,
                axum::Router::new().route("/users/{login}", axum::routing::get(lookup)),
            )
            .await
            .unwrap()
        });
        let providers = IdentityProviders::with_github_test_endpoints(
            "https://ssmcp.test",
            crate::identity::GitHubConfig {
                client_id: "fixture".into(),
                client_secret: "fixture-secret".into(),
            },
            &base,
        )
        .unwrap();
        (providers, task)
    }

    #[tokio::test]
    async fn session_access_resolves_subjects_and_isolates_grants_and_revocations() {
        let broker = Broker::new("https://ssmcp.test".into());
        let first = session(&broker, "one").await;
        let second = session(&broker, "two").await;
        let (providers, task) = provider().await;
        let owner = identity("42", "alice");
        let bob = Authorization::Identity(identity("7", "a-new-login"));
        let forged = Authorization::Identity(identity("999", "bob"));
        let add = first
            .access_command(
                &providers,
                &owner,
                AccessCommand::Add {
                    principal: "bob@github".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(add.entries.len(), 2);
        assert!(add.entries[0].owner);
        assert_eq!(
            broker.clients(IDENTITY_RESOURCE, &bob).await.unwrap().len(),
            1
        );
        assert!(
            broker
                .client(IDENTITY_RESOURCE, &bob, second.client_id())
                .await
                .is_err()
        );
        assert!(
            broker
                .client(IDENTITY_RESOURCE, &forged, first.client_id())
                .await
                .is_err()
        );
        assert!(
            first
                .call_with_authorization(
                    Operation::Status {},
                    Some(Controller {
                        name: Some("alice".into()),
                        model: None,
                        source: ControllerSource::CallerReported
                    }),
                    &forged
                )
                .await
                .unwrap_err()
                .contains("revoked")
        );
        let held = broker
            .client(IDENTITY_RESOURCE, &bob, first.client_id())
            .await
            .unwrap();
        second
            .access_command(
                &providers,
                &owner,
                AccessCommand::Add {
                    principal: "bob@github".into(),
                },
            )
            .await
            .unwrap();
        first
            .access_command(
                &providers,
                &owner,
                AccessCommand::Remove {
                    principal: "bob@github".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            broker
                .client(IDENTITY_RESOURCE, &bob, first.client_id())
                .await
                .is_err()
        );
        assert!(
            held.call_with_authorization(Operation::Status {}, None, &bob)
                .await
                .unwrap_err()
                .contains("revoked")
        );
        assert!(
            broker
                .client(IDENTITY_RESOURCE, &bob, second.client_id())
                .await
                .is_ok()
        );
        assert_eq!(first.owner_subject.as_deref(), Some("42"));
        assert_eq!(second.owner_subject.as_deref(), Some("42"));
        assert!(
            first
                .access_command(&providers, &identity("7", "bob"), AccessCommand::List {})
                .await
                .is_err()
        );
        task.abort();
    }

    #[tokio::test]
    async fn session_access_owner_duplicates_bad_names_and_capacity_fail_closed() {
        let broker = Broker::new("https://ssmcp.test".into());
        let session = session(&broker, "one").await;
        let (providers, task) = provider().await;
        let owner = identity("42", "alice");
        assert!(
            session
                .access_command(
                    &providers,
                    &owner,
                    AccessCommand::Remove {
                        principal: "alice@github".into()
                    }
                )
                .await
                .unwrap_err()
                .contains("所有者")
        );
        for principal in [
            "bad@other",
            "-bad@github",
            "missing@github",
            "imposter@github",
        ] {
            assert!(
                session
                    .access_command(
                        &providers,
                        &owner,
                        AccessCommand::Add {
                            principal: principal.into()
                        }
                    )
                    .await
                    .is_err(),
                "{principal}"
            );
            assert_eq!(session.allowed().len(), 1);
        }
        for id in 1..MAX_ALLOWED_PRINCIPALS {
            session
                .access_command(
                    &providers,
                    &owner,
                    AccessCommand::Add {
                        principal: format!("u{id}@github"),
                    },
                )
                .await
                .unwrap();
        }
        let duplicate = session
            .access_command(
                &providers,
                &owner,
                AccessCommand::Add {
                    principal: "u1@github".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(duplicate.entries.len(), MAX_ALLOWED_PRINCIPALS);
        assert!(duplicate.message.unwrap().contains("已在"));
        assert!(
            session
                .access_command(
                    &providers,
                    &owner,
                    AccessCommand::Add {
                        principal: "u32@github".into()
                    }
                )
                .await
                .unwrap_err()
                .contains("32")
        );
        assert_eq!(session.allowed().len(), MAX_ALLOWED_PRINCIPALS);
        assert!(
            session
                .access_command(
                    &providers,
                    &owner,
                    AccessCommand::Remove {
                        principal: "missing@github".into()
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(session.owner_subject.as_deref(), Some("42"));
        task.abort();
    }
}
