//! Explicit recipient groups use the existing authenticated single-message transport.
//! Each child keeps its own request ID and receipt; no name or wildcard is expanded.
use super::*;
use sha2::{Digest, Sha256};

pub(super) const MAX_TARGETS: usize = 32;
const MAX_GROUPS: usize = 128;

#[derive(Clone)]
struct Target {
    recipient: Recipient,
    request_id: String,
    started: bool,
    error: Option<String>,
    last: Option<Value>,
}

pub(super) struct Group {
    request_id: String,
    text: String,
    connection: u64,
    targets: Vec<Target>,
}

fn same_account(a: &Recipient, b: &Recipient) -> bool {
    a.id == b.id
        && a.account.provider == b.account.provider
        && a.account.subject == b.account.subject
}

fn child_id(group: &str, recipient: &str) -> String {
    let canonical = serde_json::to_vec(&(group, recipient)).expect("group key serializes");
    format!("group-{:x}", Sha256::digest(canonical))
}

fn receipt_view(message: &Outgoing) -> Value {
    json!({
        "request_id":message.request_id,"recipient_id":message.recipient_id,
        "status":message.status,"message_id":message.message_id,"error":message.error,
        "connection_closed":message.connection_closed,
        "confirmed_status":message.confirmed_status,
    })
}

impl Manager {
    pub(super) fn group_send(
        &mut self,
        recipients: &[Recipient],
        text: &str,
        request_id: &str,
    ) -> Result<(), String> {
        crate::wire::messages::validate_request_id(request_id)?;
        crate::wire::messages::validate_text(text)?;
        if recipients.is_empty() || recipients.len() > MAX_TARGETS {
            return Err("Choose 1–32 exact process inboxes or threads for this group".into());
        }
        let mut recipients = recipients.to_vec();
        recipients.sort_by(|a, b| a.id.cmp(&b.id));
        for recipient in &recipients {
            recipient.validate()?;
        }
        if recipients.windows(2).any(|pair| pair[0].id == pair[1].id) {
            return Err("A group cannot contain the same recipient twice".into());
        }
        if let Some(group) = self
            .groups
            .iter()
            .find(|group| group.request_id == request_id)
        {
            if group.text != text
                || group.targets.len() != recipients.len()
                || group
                    .targets
                    .iter()
                    .zip(&recipients)
                    .any(|(target, recipient)| !same_account(&target.recipient, recipient))
            {
                return Err(
                    "The group request ID is bound to its original text and exact recipients"
                        .into(),
                );
            }
            if group.connection != self.group_connection {
                return Err(
                    "This group belongs to an earlier connection; it was not resent".into(),
                );
            }
            return Ok(());
        }
        if !self.available() {
            return Err(UNSUPPORTED.into());
        }
        if self.groups.len() >= MAX_GROUPS {
            return Err(
                "Group receipt history is full for this client; existing groups remain inspectable"
                    .into(),
            );
        }
        // Validate the complete selection before accepting a local group plan.
        // Each child is checked again immediately before transport admission.
        for recipient in &recipients {
            self.group_recipient_current(recipient)?;
        }
        self.groups.push(Group {
            request_id: request_id.into(),
            text: text.into(),
            connection: self.group_connection,
            targets: recipients
                .into_iter()
                .map(|recipient| Target {
                    request_id: child_id(request_id, &recipient.id),
                    recipient,
                    started: false,
                    error: None,
                    last: None,
                })
                .collect(),
        });
        self.changed();
        self.pump_groups();
        Ok(())
    }

    fn group_recipient_current(&self, expected: &Recipient) -> Result<(), String> {
        let current = self
            .recipients
            .iter()
            .find(|recipient| recipient.id == expected.id)
            .ok_or("Recipient is no longer listed; refresh and select it again")?;
        if !same_account(current, expected) {
            return Err("Recipient account changed; refresh and select it again".into());
        }
        if !current.online || !current.sendable {
            return Err(
                "Recipient is currently unavailable; no other recipient was substituted".into(),
            );
        }
        Ok(())
    }

    pub(super) fn group_retry(&mut self, request_id: &str) -> Result<(), String> {
        if !self.available() {
            return Err(UNSUPPORTED.into());
        }
        self.sync_group_receipts();
        let group = self
            .groups
            .iter_mut()
            .find(|group| group.request_id == request_id)
            .ok_or("Group receipt is no longer retained")?;
        if group.connection != self.group_connection {
            return Err("The group connection ended; review its receipts and select recipients for a new message".into());
        }
        let mut retries = 0;
        for target in &mut group.targets {
            let failed = target.error.is_some()
                || target
                    .last
                    .as_ref()
                    .is_some_and(|last| last["status"] == "failed");
            let delivered = target.last.as_ref().is_some_and(|last| {
                matches!(
                    last["confirmed_status"].as_str(),
                    Some("queued" | "included" | "acknowledged" | "expired")
                ) || last["connection_closed"] == true
            });
            if failed && !delivered {
                target.started = false;
                target.error = None;
                retries += 1;
            }
        }
        if retries == 0 {
            return Err(
                "No unconfirmed failed targets can be retried; accepted targets are never resent"
                    .into(),
            );
        }
        self.changed();
        self.pump_groups();
        Ok(())
    }

    pub(super) fn sync_group_receipts(&mut self) {
        for group in &mut self.groups {
            for target in &mut group.targets {
                if let Some(message) = self.outgoing.iter().find(|message| {
                    message.request_id == target.request_id
                        && message.recipient_id == target.recipient.id
                }) {
                    target.last = Some(receipt_view(message));
                }
            }
        }
    }

    pub(super) fn close_groups(&mut self) {
        self.sync_group_receipts();
        for group in &mut self.groups {
            for target in &mut group.targets {
                if !target.started && target.error.is_none() {
                    target.error =
                        Some("Connection ended before dispatch; this target was not sent".into());
                }
            }
        }
    }

    pub(super) fn pump_groups(&mut self) {
        if !self.available() {
            return;
        }
        loop {
            if self.pending.len() >= 16
                || self
                    .sender
                    .as_ref()
                    .is_some_and(|sender| !sender.is_closed() && sender.capacity() == 0)
            {
                break;
            }
            let next = self
                .groups
                .iter()
                .enumerate()
                .filter(|(_, group)| group.connection == self.group_connection)
                .find_map(|(group_index, group)| {
                    group
                        .targets
                        .iter()
                        .enumerate()
                        .find(|(_, target)| !target.started && target.error.is_none())
                        .map(|(target_index, target)| {
                            (
                                group_index,
                                target_index,
                                target.clone(),
                                group.text.clone(),
                            )
                        })
                });
            let Some((group_index, target_index, target, text)) = next else {
                break;
            };
            let result = self
                .group_recipient_current(&target.recipient)
                .and_then(|()| self.send(&target.recipient.id, &text, &target.request_id));
            let target = &mut self.groups[group_index].targets[target_index];
            match result {
                Ok(()) => target.started = true,
                Err(error) => target.error = Some(error),
            }
            self.changed();
        }
        self.sync_group_receipts();
    }

    pub(super) fn group_snapshots(&self) -> Vec<Value> {
        self.groups.iter().map(|group| {
            let targets: Vec<Value> = group.targets.iter().map(|target| {
                let current = self.outgoing.iter()
                    .find(|message| message.request_id == target.request_id
                        && message.recipient_id == target.recipient.id)
                    .map(receipt_view)
                    .or_else(|| target.last.clone());
                let mut result = current.unwrap_or_else(|| json!({
                    "request_id":target.request_id,"recipient_id":target.recipient.id,
                    "status":"waiting","message_id":null,"error":null,
                    "connection_closed":false,"confirmed_status":null,
                }));
                if let Some(error) = &target.error {
                    result["status"] = json!("failed");
                    result["error"] = json!(error);
                }
                result["name"] = json!(target.recipient.display_name);
                result["account"] = json!(target.recipient.account);
                result
            }).collect();
            json!({
                "request_id":group.request_id,"text":group.text,"targets":targets,
                "connection_closed":group.connection != self.group_connection || !self.connected,
            })
        }).collect()
    }
}

#[cfg(test)]
mod tests;
