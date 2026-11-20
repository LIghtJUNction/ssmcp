//! Public broadcast requests expand into the existing authenticated send path.
use super::*;
use crate::messaging::Broadcast;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Arguments {
    recipient_ids: Vec<String>,
    text: String,
    request_id: String,
}

pub(super) struct Prepared {
    arguments: Arguments,
    broadcast: Broadcast,
}

pub(super) enum Request {
    Single(Command),
    Broadcast(Prepared),
}

impl Request {
    pub(super) fn parse(mut fields: serde_json::Map<String, Value>) -> Result<Self, String> {
        if fields.get("action").and_then(Value::as_str) == Some("broadcast") {
            fields.remove("action");
            let arguments: Arguments = serde_json::from_value(Value::Object(fields))
                .map_err(|_| "invalid broadcast action or arguments")?;
            let broadcast = Broadcast::new(
                arguments.request_id.clone(),
                arguments.recipient_ids.clone(),
                &arguments.text,
            )?;
            Ok(Self::Broadcast(Prepared {
                arguments,
                broadcast,
            }))
        } else {
            let command: Command = serde_json::from_value(Value::Object(fields))
                .map_err(|_| "invalid native message action or arguments")?;
            command.validate()?;
            Ok(Self::Single(command))
        }
    }
}

pub(super) struct PreparedMessage<'a> {
    pub(super) command: Command,
    pub(super) broadcast: Option<&'a Broadcast>,
}

impl From<Command> for PreparedMessage<'_> {
    fn from(command: Command) -> Self {
        Self {
            command,
            broadcast: None,
        }
    }
}

impl Broker {
    pub(super) async fn dispatch_broadcast(
        self: &Arc<Self>,
        source: &Arc<Session>,
        authorization: Authorization,
        sender: Sender,
        binding: Option<&crate::code_mode::RunBinding>,
        prepared: Prepared,
        admission: Admission<'_>,
    ) -> Result<Value, String> {
        if !self.message_authorized(source, &authorization) {
            return Err("source access changed before broadcast reservation".into());
        }
        let Prepared {
            arguments,
            broadcast,
        } = prepared;
        self.native_messages
            .reserve_broadcast(source, &sender, &broadcast)?;
        let mut outcomes = Vec::with_capacity(arguments.recipient_ids.len());
        for recipient_id in arguments.recipient_ids {
            let result: Result<messages::Receipt, String> = async {
                let current = match admission {
                    Admission::OAuth { user, token } => self
                        .authenticate_now(user, token)
                        .ok_or("authorization expired before broadcast decision")?,
                    Admission::Device(_) => {
                        return Err("broadcast requires OAuth account authorization".into());
                    }
                };
                if !self.message_authorized(source, &current) {
                    return Err("source access changed before broadcast decision".into());
                }
                if let Some(binding) = binding {
                    source
                        .collaboration
                        .bind(&binding.owner, &binding.agent_id, &binding.token)?;
                }
                if let Some(receipt) = self.native_messages.broadcast_receipt(
                    &sender,
                    &broadcast,
                    &recipient_id,
                    |proof, recipient| self.outgoing_message_live(source, proof, recipient),
                )? {
                    return Ok(receipt);
                }
                let result = self
                    .dispatch_message(
                        source,
                        current,
                        sender.clone(),
                        binding,
                        PreparedMessage {
                            command: Command::Send {
                                recipient_id: recipient_id.clone(),
                                text: arguments.text.clone(),
                                request_id: broadcast.child_request_id(&recipient_id),
                            },
                            broadcast: Some(&broadcast),
                        },
                        admission,
                    )
                    .await?;
                result
                    .receipts
                    .into_iter()
                    .next()
                    .ok_or("broadcast target returned no receipt".into())
            }
            .await;
            outcomes.push(match result {
                Ok(receipt) => json!({"recipient_id":recipient_id,"ok":true,"receipt":receipt}),
                Err(error) => json!({"recipient_id":recipient_id,"ok":false,"error":error}),
            });
        }
        let succeeded = outcomes
            .iter()
            .filter(|outcome| outcome["ok"] == true)
            .count();
        Ok(json!({
            "request_id":arguments.request_id,
            "succeeded":succeeded,
            "failed":outcomes.len() - succeeded,
            "outcomes":outcomes,
        }))
    }
}
