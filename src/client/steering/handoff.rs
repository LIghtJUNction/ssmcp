//! Operator-authorized continuity for a control session without a worker pool.
use super::*;
use crate::runtime::call_scope::Source;
use std::collections::BTreeMap;

pub(crate) const TOOL: &str = "ssmcp.ui.handoff";
const TTL: Duration = Duration::from_secs(600);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Member {
    connection: u64,
    subject: String,
    id: String,
}

#[derive(Clone)]
struct Offer {
    member: Member,
    id: String,
    created: Instant,
    payload: Value,
}

#[derive(Default)]
pub(super) struct Handoffs {
    members: BTreeMap<String, Member>,
    pending: Option<Offer>,
    accepted: Option<Offer>,
}

impl Handoffs {
    pub(super) fn coordinator(&self) -> Option<&str> {
        self.accepted
            .as_ref()
            .map(|receipt| receipt.member.id.as_str())
    }
}

fn is_main(agent: &SteeringAgent) -> bool {
    agent.role.as_deref().is_some_and(|role| {
        ["main", "coordinator", "lead", "controller"]
            .iter()
            .any(|r| role.eq_ignore_ascii_case(r))
    }) && !matches!(agent.status.as_str(), "offline" | "retiring")
}

impl Manager {
    fn live_main(&self, id: &str) -> bool {
        self.agents.iter().any(|a| a.agent_id == id && is_main(a))
    }

    fn handoff_member(
        &mut self,
        source: &Source,
        args: Value,
        now: Instant,
    ) -> Result<Value, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Accept {
            handoff_id: Option<String>,
        }
        let request: Accept =
            serde_json::from_value(args).map_err(|_| "invalid handoff arguments")?;
        let call = source
            .call
            .as_ref()
            .ok_or("bind your control participant")?;
        let thread = call.thread().ok_or("bind your control participant")?;
        let account = &call
            .attribution()
            .ok_or("verified account required")?
            .account;
        let id = thread.thread_id();
        if !self.queue_connected()
            || !self.ready
            || self.outbox.room_id.as_deref() != Some(thread.session_id())
            || call.client_id() != Some(thread.session_id())
            || account.provider != "github"
            || !self.live_main(id)
        {
            return Err(
                "the bound caller must be a live main participant in this control session".into(),
            );
        }
        let member = Member {
            connection: call.connection(),
            subject: account.subject.clone(),
            id: id.into(),
        };
        let Some(key) = request.handoff_id else {
            self.handoffs
                .members
                .retain(|id, _| self.agents.iter().any(|a| &a.agent_id == id));
            self.handoffs.members.insert(id.into(), member);
            return Ok(json!({"participant":id,"control":thread.session_id()}));
        };
        if let Some(done) = &self.handoffs.accepted
            && done.id == key
            && done.member == member
        {
            return Ok(json!({"handoff":done.payload,"duplicate":true}));
        }
        let offer = self
            .handoffs
            .pending
            .as_ref()
            .filter(|v| v.id == key && v.member == member && now.duration_since(v.created) < TTL)
            .ok_or("no current local invitation for this bound participant")?;
        let receipt = json!({"handoff":offer.payload,"duplicate":false});
        self.handoffs.accepted = self.handoffs.pending.take();
        Ok(receipt)
    }

    fn offer_control_handoff(
        &mut self,
        control: &str,
        workspace: &str,
        successor: &str,
        note: &str,
        original_subject: Option<&str>,
        now: Instant,
    ) -> Result<String, String> {
        let (from, successor) = successor
            .split_once('>')
            .map_or((None, successor), |(a, b)| (Some(a), b));
        if !self.queue_connected()
            || !self.ready
            || self.outbox.room_id.as_deref() != Some(control)
            || !self.live_main(successor)
        {
            return Err("select a live main participant in this control session".into());
        }
        if note.is_empty() || note.len() > 2048 || note.chars().any(char::is_control) {
            return Err(
                "supply progress, pending work and unknown outcomes in one line up to 2048 bytes"
                    .into(),
            );
        }
        let next = self
            .handoffs
            .members
            .get(successor)
            .ok_or("ask the successor to call ssmcp.ui.handoff {} with its own binding first")?
            .clone();
        if original_subject != Some(next.subject.as_str()) {
            return Err("handoff requires the original authorized control account".into());
        }
        let predecessor = if let Some(current) = &self.handoffs.accepted {
            if from.is_some_and(|id| id != current.member.id) {
                return Err("the named predecessor is not the current coordinator".into());
            }
            &current.member
        } else {
            let mut others = self.agents.iter().filter(|a| {
                a.agent_id != successor && is_main(a) && from.is_none_or(|id| id == a.agent_id)
            });
            let prior = others.next();
            if others.next().is_some() {
                return Err("multiple possible predecessors; specify the original main with /invite handoff FROM_ID>TO_ID note".into());
            }
            if let Some(prior) = prior {
                self.handoffs.members.get(&prior.agent_id).ok_or(
                    "ask the original main to call ssmcp.ui.handoff {} with its own binding",
                )?
            } else {
                if from.is_some() {
                    return Err("the named predecessor must be a live main participant".into());
                }
                return self.save_control_offer(control, workspace, "main", next, note, now);
            }
        };
        if predecessor.id == successor
            || predecessor.subject != next.subject
            || predecessor.connection != next.connection
        {
            return Err(
                "handoff requires distinct participants of the same authenticated control account"
                    .into(),
            );
        }
        let previous = predecessor.id.clone();
        self.save_control_offer(control, workspace, &previous, next, note, now)
    }

    fn save_control_offer(
        &mut self,
        control: &str,
        workspace: &str,
        previous: &str,
        next: Member,
        note: &str,
        now: Instant,
    ) -> Result<String, String> {
        let id = random_id()?;
        let references: Vec<_> = self
            .outbox
            .records
            .iter()
            .filter(|record| {
                record.room_id.as_deref() == Some(control)
                    && (record.message.target == previous
                        || record
                            .message
                            .delivery
                            .as_ref()
                            .and_then(|delivery| delivery.agent_id.as_deref())
                            == Some(previous))
            })
            .map(|record| {
                json!({
                    "id": record.message.id, "revision": record.message.revision,
                    "state": record.status(true), "uncertain": record.uncertain
                })
            })
            .collect();
        let payload = json!({
            "handoff_id": id, "control": control, "workspace": workspace,
            "predecessor": previous, "successor": next.id,
            "note": note, "request_refs": references
        });
        self.handoffs.pending = Some(Offer {
            member: next,
            id: id.clone(),
            created: now,
            payload,
        });
        Ok(id)
    }
}

pub(in crate::client) fn coordinator() -> Option<String> {
    with(|manager| manager.handoffs.coordinator().map(str::to_owned))
}

pub(crate) fn call(args: Value) -> Result<Value, String> {
    let source = Source::capture();
    let receipt = with(|m| m.handoff_member(&source, args, Instant::now()))?;
    if receipt["duplicate"] == false {
        crate::client::terminal::invalidate();
    }
    Ok(receipt)
}

pub(in crate::client) fn offer(
    control: &str,
    workspace: &str,
    successor: &str,
    note: &str,
) -> Result<String, String> {
    let subject = crate::client::access::original_account(control);
    with(|m| {
        m.offer_control_handoff(
            control,
            workspace,
            successor,
            note,
            subject.as_deref(),
            Instant::now(),
        )
    })
}

pub(crate) fn tool() -> Value {
    json!({"name":TOOL,"description":"On the control session bind your own main participant. With {} identify this participant to the local operator. With handoff_id accept its named local invitation once or recover its receipt. Pending invitations expire in 10 minutes; recorded references do not execute anything.",
    "inputSchema":{"type":"object","additionalProperties":false,"properties":{"handoff_id":{"type":"string","pattern":"^[0-9a-f]{32}$","minLength":32,"maxLength":32}}},
    "annotations":{"readOnlyHint":false,"destructiveHint":false}})
}

#[cfg(test)]
mod tests;
