//! Revalidate pending notices and recover metadata after a failed native reply.
use super::{CodeRunReport, EligibleUserResponse, Operation, response_authorization};
use crate::code_mode;
use serde_json::Value;
use std::{sync::Arc, time::Duration};

/// A pending notice or native failure earns one read-only probe per client/run.
/// Its successful native receipt can establish capability without a catalog.
/// A native failure is never retried or replaced by this read-only observation.
pub(super) async fn collect(report: &CodeRunReport) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
    for client_id in report.run.targets() {
        let Some(origin) = report.run.connection(&client_id) else {
            continue;
        };
        let binding = report.run.binding(&client_id);
        let agent_id = binding.as_ref().map(|binding| binding.agent_id.as_str());
        if !report.run.claim_tool_summary_probe(&client_id, agent_id) {
            continue;
        }
        let _ = tokio::time::timeout_at(deadline, async {
            let authorization = report
                .broker
                .authenticate(&report.user, &report.access_token)
                .await
                .ok_or("authorization_changed")?;
            let session = report
                .broker
                .client(&report.user, &authorization, &client_id)
                .await
                .map_err(|_| "connection_unavailable")?;
            if !Arc::ptr_eq(&origin, &session)
                || !session.is_alive()
                || !authorization.is_live()
                || !authorization.permits(&session)
            {
                return Err("connection_or_access_changed");
            }
            let owner = crate::bridge::owner(&report.user, &authorization)
                .map_err(|_| "owner_unavailable")?;
            if let Some(binding) = &binding
                && (binding.owner != owner
                    || session
                        .collaboration
                        .bind(&owner, &binding.agent_id, &binding.token)
                        .is_err())
            {
                return Err("binding_changed");
            }
            // Binding was captured before this dispatch; a different participant
            // cannot borrow a failed call's pending notice.
            if report.run.binding(&client_id).map(|value| value.agent_id)
                != binding.as_ref().map(|value| value.agent_id.clone())
            {
                return Err("binding_changed");
            }
            let _ = code_mode::AUDIT_RUN
                .scope(
                    report.run.clone(),
                    session.call_with_authorization(
                        Operation::CallTool {
                            name: code_mode::tool_summaries::TOOL.into(),
                            arguments: serde_json::json!({"action":"list"}),
                        },
                        None,
                        &authorization,
                    ),
                )
                .await;
            Ok::<(), &str>(())
        })
        .await;
    }
}

/// The same capability gate applies to both interactive and passive responses.
/// Caller-supplied JSON and legacy provider metadata cannot establish capability.
pub(super) fn notices(report: &CodeRunReport, target: &EligibleUserResponse) -> Vec<Value> {
    if !target.session.tool_summary_advertised() {
        return Vec::new();
    }
    report.run.tool_summary_notices(
        target.session.client_id(),
        target
            .binding
            .as_ref()
            .map(|binding| binding.agent_id.as_str()),
    )
}

/// Append only summary notices. Do not claim steering, acknowledge questions,
/// drain native mailboxes, or advance conversation reminders during observation.
pub(super) fn commit_response(
    report: &CodeRunReport,
    response: Value,
    eligible: Vec<EligibleUserResponse>,
) -> Value {
    let mut reminders = Vec::new();
    for target in eligible {
        if response_authorization(report, &target).is_some() {
            reminders.extend(notices(report, &target));
        }
    }
    code_mode::tool_summaries::append(response, reminders)
}
