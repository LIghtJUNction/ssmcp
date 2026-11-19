//! Authenticated SSR management routes. Identity is always reconstructed from a portal session.
use super::{Portal, Problem, dashboard_view, form, only, parameters};
use crate::{
    broker::Broker,
    history::{AuditAction, AuditOutcome},
    identity::Identity,
    management::{DashboardSnapshot, ManagementClient, ManagementError, Scope},
};
use axum::{
    Router,
    extract::{RawQuery, Request, State},
    http::HeaderMap,
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use std::{collections::HashMap, sync::Arc};

pub(super) fn routes() -> Router<Arc<Broker>> {
    Router::new()
        .route("/account/sessions", get(sessions))
        .route("/account/history", get(history))
        .route("/account/admin", get(admin))
        .route("/account/admin/history", get(admin_history))
        .route("/account/admin/users", post(user_policy))
        .route("/account/sessions/disconnect", post(disconnect))
}

fn management(portal: &Portal) -> Result<&ManagementClient, Problem> {
    portal
        .management
        .as_ref()
        .ok_or(Problem::ManagementDisabled)
}

fn management_error(error: ManagementError) -> Problem {
    match error {
        ManagementError::Disabled => Problem::ManagementDisabled,
        ManagementError::Forbidden => Problem::Forbidden,
        ManagementError::NotFound => Problem::SessionNotFound,
        ManagementError::Unavailable => Problem::ManagementUnavailable,
        ManagementError::Invalid => Problem::ManagementInvalid,
    }
}

async fn authenticate(portal: &Portal, headers: &HeaderMap) -> Result<(Identity, String), Problem> {
    let result = async {
        let (_, csrf, subject) = portal.session(headers)?;
        let subject = subject.ok_or(Problem::Unauthorized)?;
        let account = portal.storage(move |store| store.account(&subject)).await?;
        Ok((
            Identity {
                provider: "github".into(),
                subject: account.subject,
                login: account.github_login,
            },
            csrf,
        ))
    }
    .await;
    if let Err(problem) = &result {
        portal
            .record_audit(
                None,
                AuditAction::AccessDenied,
                if *problem == Problem::Storage {
                    AuditOutcome::Error
                } else {
                    AuditOutcome::Denied
                },
            )
            .await?;
    }
    let (identity, csrf) = result?;
    portal.charge_management(&identity.subject)?;
    Ok((identity, csrf))
}

async fn checked_snapshot(
    portal: &Portal,
    identity: &Identity,
    scope: Scope,
) -> Result<DashboardSnapshot, Problem> {
    let result = management(portal)?.snapshot(identity, scope).await;
    if matches!(&result, Err(ManagementError::Forbidden)) {
        portal
            .record_audit(
                Some(identity),
                AuditAction::AccessDenied,
                AuditOutcome::Denied,
            )
            .await?;
    }
    let snapshot = result.map_err(management_error)?;
    if matches!(scope, Scope::All) && !snapshot.is_admin {
        return Err(Problem::Forbidden);
    }
    Ok(snapshot)
}

async fn sessions(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
) -> Result<Response, Problem> {
    let (identity, csrf) = authenticate(&broker.portal, &headers).await?;
    let snapshot = checked_snapshot(&broker.portal, &identity, Scope::Own).await?;
    Ok(Html(dashboard_view::sessions(&snapshot, &csrf)).into_response())
}

fn before_parameter(query: Option<&str>) -> Result<Option<u64>, Problem> {
    let values = parameters(query.unwrap_or(""))?;
    if values.is_empty() {
        return Ok(None);
    }
    only(&values, &["before"])?;
    values["before"]
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .map(Some)
        .ok_or(Problem::Invalid)
}

async fn history_snapshot(
    broker: &Broker,
    headers: &HeaderMap,
    query: Option<&str>,
    scope: Scope,
) -> Result<DashboardSnapshot, Problem> {
    let (identity, _) = authenticate(&broker.portal, headers).await?;
    let before = before_parameter(query)?;
    let client = management(&broker.portal)?;
    let mut snapshot = checked_snapshot(&broker.portal, &identity, scope).await?;
    let page = client
        .audit(&identity, scope, 200, before)
        .await
        .map_err(management_error)?;
    snapshot.audit = page.events;
    snapshot.audit_next_before = page.next_before;
    if snapshot.is_admin && matches!(scope, Scope::All) {
        broker
            .portal
            .record_audit(
                Some(&identity),
                AuditAction::AdminViewed,
                AuditOutcome::Success,
            )
            .await?;
    }
    Ok(snapshot)
}

async fn history(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, Problem> {
    let snapshot = history_snapshot(&broker, &headers, query.as_deref(), Scope::Own).await?;
    Ok(Html(dashboard_view::history(&snapshot)).into_response())
}

async fn admin(State(broker): State<Arc<Broker>>, headers: HeaderMap) -> Result<Response, Problem> {
    let (identity, csrf) = authenticate(&broker.portal, &headers).await?;
    // The gateway is authoritative; local account metadata never grants administrator access.
    let snapshot = checked_snapshot(&broker.portal, &identity, Scope::All).await?;
    if !snapshot.is_admin {
        return Err(Problem::Forbidden);
    }
    let balances = management(&broker.portal)?
        .traffic_balances(&identity)
        .await
        .map_err(management_error)?;
    let mut users = broker.portal.storage(|store| store.summaries()).await?;
    for user in &mut users {
        user.traffic_remaining = balances.remaining.get(&user.subject).copied();
    }
    broker
        .portal
        .record_audit(
            Some(&identity),
            AuditAction::AdminViewed,
            AuditOutcome::Success,
        )
        .await?;
    Ok(Html(dashboard_view::admin(
        &snapshot,
        &users,
        &csrf,
        &identity.subject,
        &balances.period,
    ))
    .into_response())
}

// Decimal MiB/s keeps the visible unit separate from persisted bytes per second.
fn parse_rate_limit(raw: &str) -> Result<Option<u64>, Problem> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(None);
    }
    if value.len() > 64 {
        return Err(Problem::ManagementInvalid);
    }
    let (whole, fractional) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty() && fractional.is_empty()
        || !whole.bytes().all(|c| c.is_ascii_digit())
        || !fractional.bytes().all(|c| c.is_ascii_digit())
        || fractional.len() > 20
        || value.ends_with('.')
    {
        return Err(Problem::ManagementInvalid);
    }
    let whole = if whole.is_empty() {
        0
    } else {
        whole
            .parse::<u128>()
            .map_err(|_| Problem::ManagementInvalid)?
    };
    let fractional_value = if fractional.is_empty() {
        0
    } else {
        fractional
            .parse::<u128>()
            .map_err(|_| Problem::ManagementInvalid)?
    };
    if whole > 1024 || whole == 1024 && fractional_value != 0 {
        return Err(Problem::ManagementInvalid);
    }
    let denominator = 10_u128.pow(fractional.len() as u32);
    let bytes = whole * 1_048_576 + (fractional_value * 1_048_576 + denominator / 2) / denominator;
    if !(1..=1_073_741_824).contains(&bytes) {
        return Err(Problem::ManagementInvalid);
    }
    Ok(Some(bytes as u64))
}

#[derive(Debug, Eq, PartialEq)]
struct UserPolicyChange {
    subject: String,
    banned: Option<bool>,
    rate_limit_bps: Option<Option<u64>>,
}

fn policy_change(
    values: &HashMap<String, String>,
    actor_subject: &str,
) -> Result<UserPolicyChange, Problem> {
    let action = values.get("action").ok_or(Problem::ManagementInvalid)?;
    let (banned, rate_limit_bps) = match action.as_str() {
        "rate" => {
            only(values, &["csrf", "subject", "action", "rate_mib"])?;
            (None, Some(parse_rate_limit(&values["rate_mib"])?))
        }
        "ban" | "unban" => {
            only(values, &["csrf", "subject", "action"])?;
            (Some(action == "ban"), None)
        }
        _ => return Err(Problem::ManagementInvalid),
    };
    let subject = &values["subject"];
    if !crate::management::valid_subject(subject) {
        return Err(Problem::ManagementInvalid);
    }
    if subject == actor_subject && banned == Some(true) {
        return Err(Problem::Forbidden);
    }
    Ok(UserPolicyChange {
        subject: subject.clone(),
        banned,
        rate_limit_bps,
    })
}

async fn user_policy(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Response, Problem> {
    let audit_identity = broker.portal.identity_for_audit(request.headers()).await;
    let result = async {
        let (headers, values) = form(request).await?;
        broker
            .portal
            .verify(&headers, &values, &broker.public_url)?;
        let (identity, _) = authenticate(&broker.portal, &headers).await?;
        checked_snapshot(&broker.portal, &identity, Scope::All).await?;
        let management = management(&broker.portal)?;
        if values.get("action").map(String::as_str) == Some("traffic") {
            only(
                &values,
                &["csrf", "subject", "action", "period", "remaining_bytes"],
            )?;
            let remaining_bytes = values["remaining_bytes"]
                .parse()
                .map_err(|_| Problem::ManagementInvalid)?;
            management
                .set_traffic_remaining(
                    &identity,
                    &values["subject"],
                    &values["period"],
                    remaining_bytes,
                )
                .await
                .map_err(management_error)?;
        } else {
            let change = policy_change(&values, &identity.subject)?;
            management
                .set_user_policy(
                    &identity,
                    &change.subject,
                    change.banned,
                    change.rate_limit_bps,
                )
                .await
                .map_err(management_error)?;
        }
        Ok(Redirect::to("/account/admin").into_response())
    }
    .await;
    if matches!(result, Err(Problem::Csrf | Problem::Unauthorized)) {
        broker
            .portal
            .record_audit(
                audit_identity.as_ref(),
                AuditAction::AccessDenied,
                AuditOutcome::Denied,
            )
            .await?;
    }
    result
}

async fn admin_history(
    State(broker): State<Arc<Broker>>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
) -> Result<Response, Problem> {
    let snapshot = history_snapshot(&broker, &headers, query.as_deref(), Scope::All).await?;
    Ok(Html(dashboard_view::history(&snapshot)).into_response())
}

async fn disconnect(
    State(broker): State<Arc<Broker>>,
    request: Request,
) -> Result<Response, Problem> {
    let audit_identity = broker.portal.identity_for_audit(request.headers()).await;
    let parsed = async {
        let (headers, values) = form(request).await?;
        only(&values, &["csrf", "session_id", "return_to"])?;
        let (_, subject) = broker
            .portal
            .verify(&headers, &values, &broker.public_url)?;
        let subject = subject.ok_or(Problem::Unauthorized)?;
        let account = broker
            .portal
            .storage(move |store| store.account(&subject))
            .await?;
        let return_to = match values["return_to"].as_str() {
            "/account/sessions" => "/account/sessions",
            "/account/admin" => "/account/admin",
            _ => return Err(Problem::Invalid),
        };
        Ok((
            Identity {
                provider: "github".into(),
                subject: account.subject,
                login: account.github_login,
            },
            values["session_id"].clone(),
            return_to,
        ))
    }
    .await;
    let (identity, session_id, return_to) = match parsed {
        Ok(value) => value,
        Err(error) => {
            broker
                .portal
                .record_audit(
                    audit_identity.as_ref(),
                    AuditAction::AccessDenied,
                    if error == Problem::Storage {
                        AuditOutcome::Error
                    } else {
                        AuditOutcome::Denied
                    },
                )
                .await?;
            return Err(error);
        }
    };
    broker.portal.charge_management(&identity.subject)?;
    management(&broker.portal)?
        .disconnect(&identity, &session_id)
        .await
        .map_err(management_error)?;
    Ok(Redirect::to(return_to).into_response())
}

#[cfg(test)]
#[path = "dashboard_tests.rs"]
mod tests;
