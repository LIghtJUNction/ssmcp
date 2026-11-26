//! Gateway-authoritative ownership, administrative views and session termination.
use super::{Authorization, Broker, Session};
use crate::{
    history::{AuditAction, AuditEvent, AuditLog, AuditOutcome, AuditPage},
    identity::Identity,
    management::{
        self, AllowedIdentity, DashboardSnapshot, DashboardStats, ManagementError,
        ManagementPolicy, Scope, SessionSnapshot,
    },
};
use std::{
    collections::HashSet,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

impl Broker {
    /// Configure before sharing the broker or accepting connections.
    pub fn configure_management(&mut self, policy: ManagementPolicy, audit: Arc<AuditLog>) {
        self.management = Some(management::State { policy, audit });
    }

    fn management_access(
        &self,
        actor: &Identity,
        scope: Scope,
    ) -> Result<&management::State, ManagementError> {
        let state = self.management.as_ref().ok_or(ManagementError::Disabled)?;
        if !management::valid_identity(actor)
            || self.user_policies.is_banned(&actor.subject)
            || (scope == Scope::All && !state.policy.is_admin(actor))
        {
            return Err(ManagementError::Forbidden);
        }
        Ok(state)
    }

    pub async fn management_snapshot(
        &self,
        actor: &Identity,
        scope: Scope,
    ) -> Result<DashboardSnapshot, ManagementError> {
        let state = self.management_access(actor, scope)?;
        let authorization = Authorization::Identity(actor.clone());
        let mut visible = Vec::new();
        let mut shared_sessions = Vec::new();
        {
            let sessions = self.sessions.read().await;
            for session in sessions
                .values()
                .flat_map(|clients| clients.values())
                .filter(|session| session.is_alive())
            {
                if scope == Scope::Own && session.owner_subject.as_deref() != Some(&actor.subject) {
                    if authorization.permits(session) {
                        shared_sessions.push(session.info());
                    }
                    continue;
                }
                visible.push(SessionSnapshot {
                    id: session.client_id().into(),
                    name: session.info.name.clone(),
                    hostname: session.info.hostname.clone(),
                    cwd: session.info.cwd.clone(),
                    mode: session.info.mode.name().into(),
                    ssh_user: session.ssh_user.clone(),
                    owner_subject: session.owner_subject.clone(),
                    connected_at: session.connected_at,
                    allowed_github: session
                        .allowed()
                        .iter()
                        .filter(|id| id.provider == "github")
                        .map(|id| AllowedIdentity {
                            subject: id.subject.clone(),
                            login: id.login.clone(),
                        })
                        .collect(),
                });
            }
        }
        shared_sessions.sort_unstable_by(|a, b| a.id.cmp(&b.id));
        visible.sort_unstable_by(|a, b| {
            b.connected_at
                .cmp(&a.connected_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        let stats = DashboardStats {
            active_sessions: visible.len(),
            owners: visible
                .iter()
                .map(|s| s.ssh_user.as_str())
                .collect::<HashSet<_>>()
                .len(),
            hosts: visible
                .iter()
                .map(|s| s.hostname.as_str())
                .collect::<HashSet<_>>()
                .len(),
        };
        let page = self.management_audit(actor, scope, 50, None).await?;
        let traffic = match self.traffic_quota(&actor.subject).await {
            Ok(quota) => Some(quota),
            Err(crate::portal::Problem::Disabled) => None,
            Err(_) => return Err(ManagementError::Unavailable),
        };
        Ok(DashboardSnapshot {
            is_admin: state.policy.is_admin(actor),
            scope,
            generated_at: management::now(),
            sessions: visible,
            shared_sessions,
            stats,
            audit: page.events,
            audit_next_before: page.next_before,
            retention_days: state.audit.retention_days(),
            default_rate_limit_bps: self.user_policies.default_rate(),
            traffic,
        })
    }

    pub async fn management_audit(
        &self,
        actor: &Identity,
        scope: Scope,
        limit: usize,
        before: Option<u64>,
    ) -> Result<AuditPage, ManagementError> {
        let state = self.management_access(actor, scope)?;
        if !(1..=200).contains(&limit) {
            return Err(ManagementError::Invalid);
        }
        let audit = state.audit.clone();
        let subject = (scope == Scope::Own).then(|| actor.subject.clone());
        tokio::task::spawn_blocking(move || audit.query(subject.as_deref(), limit, before))
            .await
            .map_err(|_| ManagementError::Unavailable)?
            .map_err(|_| ManagementError::Unavailable)
    }

    /// Terminates the actual gateway wire connection, then waits for registry
    /// cleanup. A separately configured client watchdog may establish a new ID.
    pub async fn management_disconnect(
        &self,
        actor: &Identity,
        id: &str,
    ) -> Result<(), ManagementError> {
        let state = self.management_access(actor, Scope::Own)?;
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ManagementError::Invalid);
        }
        let session = self
            .sessions
            .read()
            .await
            .values()
            .find_map(|clients| clients.get(id).cloned())
            .filter(|session| session.is_alive())
            .ok_or(ManagementError::NotFound)?;
        if !state.policy.is_admin(actor) && session.owner_subject.as_deref() != Some(&actor.subject)
        {
            let mut denied = AuditEvent::new(AuditAction::AccessDenied, AuditOutcome::Denied);
            denied.actor_subject = Some(actor.subject.clone());
            // Do not disclose another owner's identity or device metadata.
            denied.session_id = Some(id.into());
            let audit = state.audit.clone();
            tokio::task::spawn_blocking(move || audit.record(denied))
                .await
                .map_err(|_| ManagementError::Unavailable)?
                .map_err(|_| ManagementError::Unavailable)?;
            return Err(ManagementError::NotFound);
        }
        let mut event = session.audit_event(
            AuditAction::SessionDisconnectRequested,
            AuditOutcome::Success,
        );
        event.actor_subject = Some(actor.subject.clone());
        let audit = state.audit.clone();
        tokio::task::spawn_blocking(move || audit.record(event))
            .await
            .map_err(|_| ManagementError::Unavailable)?
            .map_err(|_| ManagementError::Unavailable)?;
        session.alive.store(false, Ordering::Release);
        session.disconnect.notify_one();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = session.closed_notify.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if session.closed.load(Ordering::Acquire) {
                    break;
                }
                notified.await;
            }
        })
        .await
        .map_err(|_| ManagementError::Unavailable)?;
        Ok(())
    }

    pub(crate) async fn management_traffic_balances(
        &self,
        actor: &Identity,
    ) -> Result<management::TrafficBalances, ManagementError> {
        let state = self.management_access(actor, Scope::All)?;
        if !self.portal.has_storage() {
            return Ok(management::TrafficBalances {
                period: String::new(),
                remaining: Default::default(),
            });
        }
        let policy = state.policy.clone();
        let offset = self.traffic_utc_offset_minutes.load(Ordering::Acquire);
        self.portal
            .storage(move |store| {
                let period = crate::user_policy::traffic_period_at(management::now(), offset);
                let remaining = store
                    .summaries()?
                    .into_iter()
                    .filter(|account| !policy.is_admin_subject(&account.subject))
                    .map(|account| {
                        let bytes =
                            store.admin_traffic_remaining(&account.subject, &period, offset)?;
                        Ok((account.subject, bytes))
                    })
                    .collect::<Result<_, _>>()?;
                Ok(management::TrafficBalances { period, remaining })
            })
            .await
            .map_err(policy_error)
    }

    pub(crate) async fn management_set_traffic_remaining(
        &self,
        actor: &Identity,
        subject: &str,
        period: &str,
        remaining_bytes: u64,
    ) -> Result<(), ManagementError> {
        let _update = self.user_policy_updates.lock().await;
        let state = self.management_access(actor, Scope::All)?;
        if state.policy.is_admin_subject(subject) {
            return Err(ManagementError::Forbidden);
        }
        if !management::valid_subject(subject) || !crate::user_policy::valid_traffic_period(period)
        {
            return Err(ManagementError::Invalid);
        }
        let mut event = AuditEvent::new(AuditAction::TrafficBalanceChanged, AuditOutcome::Pending);
        event.actor_subject = Some(actor.subject.clone());
        event.owner_subject = Some(subject.into());
        let audit = state.audit.clone();
        let subject = subject.to_owned();
        let period = period.to_owned();
        let offset = self.traffic_utc_offset_minutes.load(Ordering::Acquire);
        self.portal
            .storage(move |store| {
                if period != crate::user_policy::traffic_period_at(management::now(), offset) {
                    return Err(crate::portal::Problem::Invalid);
                }
                store.account_record(&subject)?;
                audit
                    .record(event.clone())
                    .map_err(|_| crate::portal::Problem::Storage)?;
                let result =
                    store.set_traffic_remaining(&subject, &period, offset, remaining_bytes);
                event.outcome = if result.is_ok() {
                    AuditOutcome::Success
                } else {
                    AuditOutcome::Error
                };
                audit
                    .record(event)
                    .map_err(|_| crate::portal::Problem::Storage)?;
                result
            })
            .await
            .map_err(policy_error)
    }

    /// Apply a partial account policy update under a single administrator
    /// mutation lock, then invalidate every live form of access on a ban.
    pub async fn management_set_user_policy(
        &self,
        actor: &Identity,
        subject: &str,
        banned: Option<bool>,
        rate_limit_bps: Option<Option<u64>>,
    ) -> Result<(), ManagementError> {
        let state = self.management_access(actor, Scope::All)?;
        let _update = self.user_policy_updates.lock().await;
        if self.user_policies.is_banned(&actor.subject) {
            return Err(ManagementError::Forbidden);
        }
        if !management::valid_subject(subject)
            || (banned.is_none() && rate_limit_bps.is_none())
            || rate_limit_bps
                .flatten()
                .is_some_and(|rate| !crate::user_policy::valid_rate(rate))
        {
            return Err(ManagementError::Invalid);
        }
        if subject == actor.subject && banned == Some(true) {
            return Err(ManagementError::Forbidden);
        }
        let target = subject.to_owned();
        self.portal
            .storage(move |store| store.account_record(&target))
            .await
            .map_err(policy_error)?;
        let audit = state.audit.clone();
        let mut event = AuditEvent::new(AuditAction::UserPolicyChanged, AuditOutcome::Pending);
        event.actor_subject = Some(actor.subject.clone());
        event.owner_subject = Some(subject.into());
        let intent = event.clone();
        tokio::task::spawn_blocking(move || audit.record(intent))
            .await
            .map_err(|_| ManagementError::Unavailable)?
            .map_err(|_| ManagementError::Unavailable)?;

        let target = subject.to_owned();
        let policies = self.user_policies.clone();
        let account = self
            .portal
            .storage(move |store| {
                let account = store.set_policy(&target, banned, rate_limit_bps)?;
                policies.apply_account(
                    &target,
                    account.banned,
                    account.rate_limit_bps,
                    account.has_recharged,
                );
                Ok(account)
            })
            .await
            .map_err(policy_error)?;
        if account.banned {
            let oauth_revocation = self.oauth.revoke_subject(subject);
            self.portal
                .revoke_browser_sessions(subject)
                .map_err(policy_error)?;
            let target = subject.to_owned();
            self.portal
                .client_storage(move |store| store.revoke_subject(&target))
                .await
                .map_err(policy_error)?;
            let active: Vec<_> = self
                .sessions
                .read()
                .await
                .values()
                .flat_map(|clients| clients.values())
                .filter(|session| {
                    !session.locally_owned_peer && session.owner_subject.as_deref() == Some(subject)
                })
                .cloned()
                .collect();
            for session in &active {
                session.alive.store(false, Ordering::Release);
                session.disconnect.notify_one();
            }
            tokio::time::timeout(Duration::from_secs(5), async {
                for session in &active {
                    loop {
                        let notified = session.closed_notify.notified();
                        tokio::pin!(notified);
                        notified.as_mut().enable();
                        if session.closed.load(Ordering::Acquire) || session.sender.is_closed() {
                            break;
                        }
                        notified.await;
                    }
                }
            })
            .await
            .map_err(|_| ManagementError::Unavailable)?;
            oauth_revocation.map_err(|_| ManagementError::Unavailable)?;
        }
        event.outcome = AuditOutcome::Success;
        let audit = state.audit.clone();
        tokio::task::spawn_blocking(move || audit.record(event))
            .await
            .map_err(|_| ManagementError::Unavailable)?
            .map_err(|_| ManagementError::Unavailable)?;
        Ok(())
    }

    /// Portal-originated events contain only a verified actor and an allowed
    /// action; a caller cannot append arbitrary tool arguments or result data.
    pub async fn management_record(
        &self,
        actor: Option<&Identity>,
        action: AuditAction,
        outcome: AuditOutcome,
    ) -> Result<(), ManagementError> {
        let state = self.management.as_ref().ok_or(ManagementError::Disabled)?;
        if let Some(actor) = actor {
            if !management::valid_identity(actor) {
                return Err(ManagementError::Forbidden);
            }
            if !matches!(
                action,
                AuditAction::Login
                    | AuditAction::Logout
                    | AuditAction::KeyAdded
                    | AuditAction::KeyRemoved
                    | AuditAction::AccessDenied
                    | AuditAction::AdminViewed
            ) || (matches!(action, AuditAction::AdminViewed) && !state.policy.is_admin(actor))
            {
                return Err(ManagementError::Forbidden);
            }
        } else if !matches!(action, AuditAction::Login | AuditAction::AccessDenied)
            || !matches!(outcome, AuditOutcome::Denied | AuditOutcome::Error)
        {
            return Err(ManagementError::Forbidden);
        }
        let mut event = AuditEvent::new(action, outcome);
        event.actor_subject = actor.map(|actor| actor.subject.clone());
        event.owner_subject = event.actor_subject.clone();
        let audit = state.audit.clone();
        tokio::task::spawn_blocking(move || audit.record(event))
            .await
            .map_err(|_| ManagementError::Unavailable)?
            .map_err(|_| ManagementError::Unavailable)?;
        Ok(())
    }
}

fn policy_error(error: crate::portal::Problem) -> ManagementError {
    use crate::portal::Problem;
    match error {
        Problem::Invalid | Problem::UsernameInvalid => ManagementError::Invalid,
        Problem::Unauthorized => ManagementError::NotFound,
        Problem::Forbidden | Problem::Banned => ManagementError::Forbidden,
        Problem::Disabled => ManagementError::Disabled,
        _ => ManagementError::Unavailable,
    }
}

fn display_metadata(value: &str) -> Option<String> {
    let mut bounded = String::new();
    for c in value.chars().filter(|c| !c.is_control()) {
        if bounded.len() + c.len_utf8() > 256 {
            break;
        }
        bounded.push(c);
    }
    (!bounded.is_empty()).then_some(bounded)
}

impl Session {
    pub(super) fn audit_event(&self, action: AuditAction, outcome: AuditOutcome) -> AuditEvent {
        let mut event = AuditEvent::new(action, outcome);
        event.owner_subject = self.owner_subject.clone();
        event.session_id = Some(self.client_id().into());
        event.client_name = display_metadata(&self.info.name);
        event.hostname = display_metadata(&self.info.hostname);
        event
    }

    pub(super) async fn audit_record(&self, event: AuditEvent) -> Result<(), String> {
        let Some(audit) = self.audit.clone() else {
            return Ok(());
        };
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            tokio::task::spawn_blocking(move || audit.record(event)),
        )
        .await;
        if matches!(result, Ok(Ok(Ok(_)))) {
            Ok(())
        } else {
            crate::observability::log(
                "error",
                "audit",
                &[("event", "history_write_failed".into())],
            );
            Err("audit storage unavailable".into())
        }
    }

    pub(super) async fn audit_call(
        &self,
        tool_name: Option<&str>,
        actor_subject: Option<&str>,
        attribution: Option<&crate::wire::ControlAttribution>,
        run_id: Option<&str>,
        outcome: AuditOutcome,
    ) -> Result<(), String> {
        if self.audit.is_none() {
            return Ok(());
        }
        let mut event = self.audit_event(AuditAction::ToolCall, outcome);
        event.actor_subject = actor_subject.map(str::to_owned);
        event.attribution = attribution.cloned();
        event.run_id = run_id.map(str::to_owned);
        event.tool_name = tool_name
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= 256
                    && name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_./:-".contains(&b))
            })
            .map(str::to_owned);
        self.audit_record(event).await
    }
}

#[cfg(test)]
mod tests;
