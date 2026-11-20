//! Negotiated native messaging. Display labels never resolve a recipient.
use super::{ControlAccount, ControlAgent, MAX_USER_MESSAGE_BYTES};
use serde::{Deserialize, Serialize};

pub const CAPABILITY: &str = "native_messages_v1";
pub const MAX_RECIPIENTS: usize = 512;
pub const MAX_MESSAGES: usize = 128;
pub const MAX_DELIVERY: usize = 32;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RecipientKind {
    AccountInbox,
    Agent,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Recipient {
    pub id: String,
    pub client_id: String,
    pub agent_id: Option<String>,
    pub display_name: String,
    pub kind: RecipientKind,
    pub account: ControlAccount,
    pub online: bool,
    pub sendable: bool,
}

pub fn parse_recipient(value: &str) -> Result<(&str, Option<&str>), String> {
    let parts: Vec<_> = value.split(':').collect();
    let (client, agent) = match parts.as_slice() {
        ["client", client] => (*client, None),
        ["agent", client, agent] => (*client, Some(*agent)),
        _ => return Err("select an exact recipient returned by messages.list".into()),
    };
    if client.len() != 64 || !client.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("recipient client identifier is invalid".into());
    }
    if let Some(agent) = agent {
        let suffix = agent.strip_prefix("agent_").unwrap_or("");
        if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("recipient agent identifier is invalid".into());
        }
    }
    Ok((client, agent))
}

pub fn validate_request_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
    {
        return Err(
            "request_id must be 1..=128 ASCII letters, digits, dots, underscores or hyphens".into(),
        );
    }
    Ok(())
}

pub fn validate_text(value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > MAX_USER_MESSAGE_BYTES || value.contains('\0') {
        return Err(format!(
            "message text must be nonempty, contain no NUL and fit {MAX_USER_MESSAGE_BYTES} UTF-8 bytes"
        ));
    }
    Ok(())
}

impl Recipient {
    pub fn validate(&self) -> Result<(), String> {
        let (client, agent) = parse_recipient(&self.id)?;
        if client != self.client_id
            || agent != self.agent_id.as_deref()
            || matches!(self.kind, RecipientKind::Agent) != agent.is_some()
            || self.display_name.is_empty()
            || self.display_name.len() > 1024
            || self.display_name.chars().any(char::is_control)
        {
            return Err("native recipient descriptor is inconsistent".into());
        }
        self.account.validate()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    LocalUser,
    Agent,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Sender {
    pub kind: Origin,
    pub client_id: String,
    pub account: ControlAccount,
    pub agent: Option<ControlAgent>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Queued,
    Included,
    Acknowledged,
    Expired,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub request_id: String,
    pub message_id: String,
    pub recipient_id: String,
    pub status: Status,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub included_at_ms: Option<u64>,
    pub acknowledged_at_ms: Option<u64>,
    pub duplicate: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Message {
    pub receipt: Receipt,
    pub sender: Sender,
    pub recipient: Recipient,
    pub text: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum Command {
    List {},
    Send {
        recipient_id: String,
        text: String,
        request_id: String,
    },
    Status {
        message_id: String,
    },
    Inbox {},
    Ack {
        message_id: String,
    },
}

impl Command {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Send {
                recipient_id,
                text,
                request_id,
            } => {
                parse_recipient(recipient_id)?;
                validate_text(text)?;
                validate_request_id(request_id)
            }
            Self::Status { message_id } | Self::Ack { message_id } => {
                let suffix = message_id.strip_prefix("native_").unwrap_or("");
                if suffix.len() != 32 || !suffix.bytes().all(|b| b.is_ascii_hexdigit()) {
                    Err("message_id must come from a native message receipt".into())
                } else {
                    Ok(())
                }
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    #[serde(default)]
    pub recipients: Vec<Recipient>,
    #[serde(default)]
    pub receipts: Vec<Receipt>,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub more_recipients: bool,
}

impl Reply {
    pub fn validate(&self) -> Result<(), String> {
        if self.recipients.len() > MAX_RECIPIENTS
            || self.receipts.len() > MAX_MESSAGES
            || self.messages.len() > MAX_DELIVERY
        {
            return Err("native message reply exceeds its bounded collection limits".into());
        }
        for recipient in &self.recipients {
            recipient.validate()?;
        }
        for message in &self.messages {
            message.recipient.validate()?;
            message.sender.account.validate()?;
            if let Some(agent) = &message.sender.agent {
                agent.validate()?;
            }
            validate_text(&message.text)?;
        }
        Ok(())
    }
}
