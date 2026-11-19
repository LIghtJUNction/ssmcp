//! Peer candidates are data, separate from user steering and model execution.
pub(crate) mod queue;

use crate::wire::{ControlAccount, ControlAgent, Controller};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

pub const VERSION: u32 = 1;
pub const MAX_TEXT_BYTES: usize = 16 * 1024;
pub const MAX_MESSAGES: usize = 128;
pub const MAX_INBOX_BYTES: usize = 256 * 1024;
pub const MAX_READ_MESSAGES: usize = 32;
pub const MAX_READ_BYTES: usize = 64 * 1024;
pub const MAX_BATCH: usize = 128;
pub const MAX_WAIT_MS: u64 = 30_000;
pub const DEFAULT_TTL_MS: u64 = 10 * 60 * 1000;
pub const MAX_TTL_MS: u64 = 60 * 60 * 1000;
pub const MAX_RECEIPTS: usize = 128;

pub fn limits() -> Value {
    json!({"max_text_bytes":MAX_TEXT_BYTES,"max_messages":MAX_MESSAGES,
        "max_inbox_bytes":MAX_INBOX_BYTES,"max_read_messages":MAX_READ_MESSAGES,
        "max_read_bytes":MAX_READ_BYTES,"max_batch":MAX_BATCH,
        "max_wait_ms":MAX_WAIT_MS,"default_ttl_ms":DEFAULT_TTL_MS,
        "max_ttl_ms":MAX_TTL_MS,"max_retained_receipts":MAX_RECEIPTS,
        "max_waiters_per_session":4,"automatic_execution":false,
        "acknowledgement":"explicit handling, not proof of human reading"})
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub process_id: String,
    pub pid: u32,
}

pub fn process_identity() -> Result<ProcessIdentity, String> {
    static PROCESS: OnceLock<Result<ProcessIdentity, String>> = OnceLock::new();
    PROCESS
        .get_or_init(|| {
            Ok(ProcessIdentity {
                process_id: random_id("process")?,
                pid: std::process::id(),
            })
        })
        .clone()
}

pub(crate) fn random_id(prefix: &str) -> Result<String, String> {
    let mut bytes = [0; 16];
    crate::platform::random_bytes(&mut bytes)
        .map_err(|_| "cannot allocate peer message identity")?;
    Ok(format!(
        "{prefix}_{}",
        bytes.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ))
}

pub(crate) fn valid_id(value: &str, prefix: &str, bytes: usize) -> bool {
    value
        .strip_prefix(prefix)
        .and_then(|v| v.strip_prefix('_'))
        .is_some_and(|v| {
            v.len() == bytes * 2
                && v.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
}

pub(crate) fn client_id(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub(crate) fn text(value: &str, label: &str, max: usize, multiline: bool) -> Result<(), String> {
    if value.trim().is_empty()
        || value.len() > max
        || value
            .chars()
            .any(|c| c.is_control() && !(multiline && matches!(c, '\n' | '\t')))
    {
        return Err(format!(
            "{label} must contain 1..={max} UTF-8 bytes without terminal controls"
        ));
    }
    Ok(())
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Principal {
    Account {
        account: ControlAccount,
    },
    Connection {
        user: String,
        anchor_client_id: String,
    },
}

impl Principal {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Account { account } => account.validate(),
            Self::Connection {
                user,
                anchor_client_id,
            } => {
                text(user, "authenticated connection user", 128, false)?;
                if !client_id(anchor_client_id) {
                    return Err("invalid authentication anchor".into());
                }
                Ok(())
            }
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct BoundParticipant {
    pub client_id: String,
    pub agent: ControlAgent,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub principal: Principal,
    pub run_id: Option<String>,
    pub participant: Option<BoundParticipant>,
    /// Optional display attribution. Never an authorization or recipient selector.
    pub controller: Option<Controller>,
}

impl Source {
    fn validate(&self) -> Result<(), String> {
        self.principal.validate()?;
        if let Some(run) = &self.run_id
            && (run.len() != 32 || !run.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err("invalid peer source run".into());
        }
        if let Some(participant) = &self.participant {
            if !client_id(&participant.client_id) {
                return Err("invalid participant client".into());
            }
            participant.agent.validate()?;
        }
        if let Some(controller) = &self.controller {
            controller.validate()?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub client_id: String,
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
}

impl Target {
    pub fn validate(&self) -> Result<(), String> {
        if !client_id(&self.client_id)
            || !valid_id(&self.session_id, "session", 16)
            || self
                .thread_id
                .as_ref()
                .is_some_and(|id| !valid_id(id, "agent", 16))
        {
            return Err(
                "use exact live client, session and registered participant IDs from messages.list"
                    .into(),
            );
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PeerMessage {
    pub message_id: String,
    pub delivery_id: String,
    pub source: Source,
    pub target: Target,
    pub text: String,
    pub topic: Option<String>,
    pub reply_to: Option<String>,
    pub sent_at_ms: u64,
    pub ttl_ms: u64,
}

impl PeerMessage {
    pub fn validate(&self) -> Result<(), String> {
        if !valid_id(&self.message_id, "peer", 16) || !valid_id(&self.delivery_id, "delivery", 32) {
            return Err("invalid peer message or delivery ID".into());
        }
        self.source.validate()?;
        self.target.validate()?;
        text(&self.text, "peer text", MAX_TEXT_BYTES, true)?;
        if let Some(topic) = &self.topic {
            text(topic, "peer topic", 128, false)?;
        }
        if self
            .reply_to
            .as_ref()
            .is_some_and(|id| !valid_id(id, "peer", 16) && !valid_id(id, "delivery", 32))
        {
            return Err("reply_to must identify a peer message or delivery".into());
        }
        if !(1..=MAX_TTL_MS).contains(&self.ttl_ms) {
            return Err("peer TTL exceeds its bound".into());
        }
        Ok(())
    }
}

/// Private wire operation. No native tool accepts an arbitrary Deliver envelope.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum NativeRequest {
    Deliver {
        message: Box<PeerMessage>,
    },
    Read {
        target: Target,
        after: u64,
        limit: usize,
    },
    Wait {
        target: Target,
        after: u64,
        limit: usize,
        wait_ms: u64,
    },
    Ack {
        target: Target,
        ids: Vec<String>,
        through: Option<u64>,
    },
    Status {
        target: Target,
        ids: Vec<String>,
    },
}

impl NativeRequest {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Deliver { message } => message.validate(),
            Self::Read { target, limit, .. } | Self::Wait { target, limit, .. } => {
                target.validate()?;
                if !(1..=MAX_READ_MESSAGES).contains(limit) {
                    return Err("peer read limit is out of range".into());
                }
                if matches!(self, Self::Wait { wait_ms, .. } if *wait_ms > MAX_WAIT_MS) {
                    return Err("peer wait exceeds 30 seconds".into());
                }
                Ok(())
            }
            Self::Ack {
                target,
                ids,
                through,
            } => {
                target.validate()?;
                validate_ids(ids)?;
                if ids.is_empty() == through.is_none() {
                    return Err("ack requires IDs or a cursor, exclusively".into());
                }
                Ok(())
            }
            Self::Status { target, ids } => {
                target.validate()?;
                validate_ids(ids)?;
                if ids.is_empty() {
                    return Err("status requires delivery IDs".into());
                }
                Ok(())
            }
        }
    }
}

fn validate_ids(ids: &[String]) -> Result<(), String> {
    if ids.len() > MAX_BATCH || ids.iter().any(|id| !valid_id(id, "delivery", 32)) {
        return Err("delivery IDs are invalid or exceed the batch limit".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
