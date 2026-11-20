//! Capture only host-generated native summary notices, never JavaScript results.
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;

const META_KEY: &str = "ssmcp.tool_summary";
pub(crate) const TOOL: &str = "ssmcp.ui.tool_summary";
pub(crate) const MAX_REMINDERS: usize = 32;
pub(crate) const MAX_NOTICE_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Reminder {
    schema_version: u64,
    group_id: String,
    revision: u64,
    call_count: u64,
    covered_call_count: u64,
    threshold: u64,
    tool: String,
}

impl Reminder {
    fn parse(value: &Value) -> Option<Self> {
        if value.to_string().len() > 1024 {
            return None;
        }
        let notice: Self = serde_json::from_value(value.clone()).ok()?;
        (notice.schema_version == 1
            && !notice.group_id.is_empty()
            && notice.group_id.len() <= 128
            && notice
                .group_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            && notice.threshold == 10
            && notice.call_count.saturating_sub(notice.covered_call_count) > notice.threshold
            && notice.covered_call_count <= notice.call_count
            && notice.tool == TOOL)
            .then_some(notice)
    }
}

#[derive(Clone)]
struct Captured {
    client_id: String,
    agent_id: Option<String>,
    reminder: Reminder,
}

#[derive(Clone)]
struct ObservedGroup {
    client_id: String,
    agent_id: Option<String>,
    group_id: String,
    revision: u64,
    call_count: u64,
    covered_call_count: u64,
}
impl ObservedGroup {
    fn parse(client_id: &str, agent_id: Option<&str>, value: &Value) -> Option<Self> {
        let group_id = value.get("group_id")?.as_str()?;
        if group_id.is_empty()
            || group_id.len() > 128
            || !group_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return None;
        }
        let revision = value.get("revision")?.as_u64()?;
        let call_count = value.get("call_count")?.as_u64()?;
        let covered_call_count = value.get("covered_call_count")?.as_u64()?;
        (covered_call_count <= call_count).then(|| Self {
            client_id: client_id.into(),
            agent_id: agent_id.map(str::to_owned),
            group_id: group_id.into(),
            revision,
            call_count,
            covered_call_count,
        })
    }
    fn matches(&self, client_id: &str, agent_id: Option<&str>, group_id: &str) -> bool {
        self.client_id == client_id
            && self.agent_id.as_deref() == agent_id
            && self.group_id == group_id
    }
    fn supersedes(&self, reminder: &Reminder) -> bool {
        reminder.revision < self.revision
            || (reminder.revision == self.revision
                && reminder.call_count.saturating_sub(self.covered_call_count)
                    <= reminder.threshold)
    }
}
#[derive(Default)]
pub(crate) struct Capture {
    notices: Vec<Captured>,
    // Per-run native receipts are revision watermarks, not a second group counter.
    observed: Vec<ObservedGroup>,
    failed: HashSet<(String, Option<String>)>,
    probed: HashSet<String>,
}
impl Capture {
    /// The broker supplies the client and binding captured before real dispatch.
    pub(crate) fn observe(&mut self, client_id: &str, agent_id: Option<&str>, native: &Value) {
        let Some(value) = native.get("_meta").and_then(|meta| meta.get(META_KEY)) else {
            return;
        };
        let Some(reminder) = Reminder::parse(value) else {
            return;
        };
        if self.observed.iter().any(|state| {
            state.matches(client_id, agent_id, &reminder.group_id) && state.supersedes(&reminder)
        }) || self.notices.iter().any(|old| {
            old.client_id == client_id
                && old.agent_id.as_deref() == agent_id
                && old.reminder.group_id == reminder.group_id
                && old.reminder.revision == reminder.revision
        }) {
            return;
        }
        if self.notices.len() < MAX_REMINDERS {
            self.notices.push(Captured {
                client_id: client_id.into(),
                agent_id: agent_id.map(str::to_owned),
                reminder,
            });
        }
    }
    /// Only an exact, successful native tool_summary reply reaches this method.
    /// JavaScript results and other native tool bodies cannot retire a notice.
    pub(crate) fn reconcile(&mut self, client_id: &str, agent_id: Option<&str>, native: &Value) {
        if native["isError"] == true {
            return;
        }
        let body = &native["structuredContent"];
        if body["schema_version"] != 1 {
            return;
        }
        if body["accepted"] == true {
            let Some(state) = ObservedGroup::parse(client_id, agent_id, body) else {
                return;
            };
            if body["needs_summary"] != false || state.covered_call_count != state.call_count {
                return;
            }
            self.notices.retain(|entry| {
                !state.matches(
                    &entry.client_id,
                    entry.agent_id.as_deref(),
                    &entry.reminder.group_id,
                ) || !state.supersedes(&entry.reminder)
            });
            self.remember(state);
            return;
        }
        let Some(groups) = body["groups"]
            .as_array()
            .filter(|_| body["threshold"] == 10)
        else {
            return;
        };
        if groups.len() > super::MAX_CALLS {
            return;
        }
        let states: Vec<_> = groups
            .iter()
            .filter_map(|group| ObservedGroup::parse(client_id, agent_id, group))
            .collect();
        // A list is the authoritative current writable scope. Missing,
        // superseded and no-longer-due groups must not survive validation.
        self.notices.retain_mut(|entry| {
            if entry.client_id != client_id || entry.agent_id.as_deref() != agent_id {
                return true;
            }
            let Some(state) = states
                .iter()
                .find(|state| state.group_id == entry.reminder.group_id)
            else {
                return body["truncated"] == true || body["has_more"] == true;
            };
            // Native replies can arrive out of order. Neither an older revision
            // nor fewer calls at the same revision may retire an earned notice.
            if state.revision < entry.reminder.revision
                || (state.revision == entry.reminder.revision
                    && state.call_count < entry.reminder.call_count)
            {
                return true;
            }
            if state.revision > entry.reminder.revision
                || state.call_count.saturating_sub(state.covered_call_count)
                    <= entry.reminder.threshold
            {
                return false;
            }
            entry.reminder.call_count = state.call_count;
            entry.reminder.covered_call_count = state.covered_call_count;
            true
        });
        for state in states {
            self.remember(state);
        }
    }
    fn remember(&mut self, state: ObservedGroup) {
        if let Some(old) = self
            .observed
            .iter_mut()
            .find(|old| old.matches(&state.client_id, state.agent_id.as_deref(), &state.group_id))
        {
            if state.revision > old.revision
                || (state.revision == old.revision
                    && state.covered_call_count >= old.covered_call_count)
            {
                *old = state;
            }
        } else if self.observed.len() < super::MAX_CALLS {
            self.observed.push(state);
        }
    }
    pub(crate) fn failed(&mut self, client_id: &str, agent_id: Option<&str>) {
        if self.failed.len() < super::MAX_CALLS {
            self.failed
                .insert((client_id.into(), agent_id.map(str::to_owned)));
        }
    }
    pub(crate) fn claim_probe(&mut self, client_id: &str, agent_id: Option<&str>) -> bool {
        (self
            .failed
            .contains(&(client_id.into(), agent_id.map(str::to_owned)))
            || self
                .notices
                .iter()
                .any(|entry| entry.client_id == client_id && entry.agent_id.as_deref() == agent_id))
            && self.probed.len() < super::MAX_CALLS
            && self.probed.insert(client_id.into())
    }
    /// Called only after current account/ACL/binding checks by the HTTP finalizer.
    pub(crate) fn for_scope(&self, client_id: &str, agent_id: Option<&str>) -> Vec<Value> {
        self.notices
            .iter()
            .filter(|entry| entry.client_id == client_id && entry.agent_id.as_deref() == agent_id)
            .map(|entry| {
                let mut notice =
                    serde_json::to_value(&entry.reminder).expect("bounded reminder serializes");
                notice["client_id"] = json!(client_id);
                notice
            })
            .collect()
    }
}

pub(crate) fn append(mut response: Value, notices: Vec<Value>) -> Value {
    if notices.is_empty() {
        return response;
    }
    let Some(root) = response.as_object_mut() else {
        return response;
    };
    if root
        .get("structuredContent")
        .is_some_and(|value| !value.is_object())
        || root.get("content").is_some_and(|value| !value.is_array())
    {
        return response;
    }
    let notices: Vec<_> = notices.into_iter().take(MAX_REMINDERS).collect();
    let encoded = serde_json::to_string(&notices).expect("bounded notices serialize");
    let text = format!(
        "TOOL SUMMARY REMINDER — More than 10 new logical tool calls have accumulated in the listed group. Call ssmcp.ui.tool_summary with action:'set', the exact group_id, a short summary and expected_revision. Update the existing group; this is a display reminder, not a new user requirement. {}",
        encoded
    );
    if text.len() + encoded.len() > MAX_NOTICE_BYTES {
        return response;
    }
    root.entry("structuredContent").or_insert_with(|| json!({}))["tool_summary_reminders"] =
        json!(notices);
    root.entry("content")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .expect("validated content array")
        .push(json!({"type":"text","text":text}));
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn native(group: &str, revision: u64, calls: u64, covered: u64) -> Value {
        json!({"content":[],"_meta":{"ssmcp.tool_summary":{
            "schema_version":1,"group_id":group,"revision":revision,
            "call_count":calls,"covered_call_count":covered,"threshold":10,"tool":TOOL
        }}})
    }

    #[test]
    fn only_host_metadata_is_captured_and_not_javascript_or_native_body_fields() {
        let mut captured = Capture::default();
        let forged = native("g", 0, 11, 0)["_meta"][META_KEY].clone();
        for value in [
            json!({"result":{"_meta":{"ssmcp.tool_summary":forged}}}),
            json!({"structuredContent":{"tool_summary_reminders":[forged]}}),
            json!({"content":[{"type":"text","text":"TOOL SUMMARY REMINDER"}]}),
            json!({"_meta":{"ssmcp.tool_summary":{"group_id":"g"}}}),
        ] {
            captured.observe("client-a", None, &value);
        }
        assert!(captured.for_scope("client-a", None).is_empty());
        captured.observe("client-a", None, &native("g", 0, 11, 0));
        assert_eq!(captured.for_scope("client-a", None).len(), 1);
    }

    #[test]
    fn threshold_revision_and_scope_are_preserved_without_duplicate_notices() {
        let mut captured = Capture::default();
        captured.observe("client-a", None, &native("g", 0, 10, 0));
        assert!(captured.for_scope("client-a", None).is_empty());
        captured.observe("client-a", None, &native("g", 0, 11, 0));
        captured.observe("client-a", None, &native("g", 0, 18, 0));
        assert_eq!(captured.for_scope("client-a", None).len(), 1);
        assert!(captured.for_scope("client-b", None).is_empty());
        assert!(captured.for_scope("client-a", Some("agent-a")).is_empty());
        captured.observe("client-a", Some("agent-a"), &native("g-agent", 1, 22, 11));
        assert_eq!(captured.for_scope("client-a", Some("agent-a")).len(), 1);
        assert!(captured.for_scope("client-a", Some("agent-b")).is_empty());
        captured.observe("client-a", None, &native("g", 1, 21, 11));
        assert_eq!(captured.for_scope("client-a", None).len(), 1);
        captured.observe("client-a", None, &native("g", 1, 22, 11));
        assert_eq!(captured.for_scope("client-a", None).len(), 2);
    }

    #[test]
    fn malformed_or_oversized_metadata_cannot_supply_instructions_or_targets() {
        let valid = native("g", 0, 11, 0);
        let mut captured = Capture::default();
        for (field, value) in [
            ("tool", json!("arbitrary.tool")),
            ("threshold", json!(0)),
            ("schema_version", json!(2)),
            ("covered_call_count", json!(50)),
            ("group_id", json!("g".repeat(129))),
            ("revision", json!(-1)),
            ("text", json!("private-fixture")),
        ] {
            let mut bad = valid.clone();
            bad["_meta"][META_KEY][field] = value;
            captured.observe("client-a", None, &bad);
        }
        assert!(captured.for_scope("client-a", None).is_empty());
    }

    #[test]
    fn failed_scope_gets_at_most_one_probe_per_client_without_cross_binding_fallback() {
        let mut captured = Capture::default();
        captured.failed("a", Some("original"));
        assert!(!captured.claim_probe("a", Some("different")));
        assert!(!captured.claim_probe("a", None));
        assert!(captured.claim_probe("a", Some("original")));
        assert!(!captured.claim_probe("a", Some("original")));
        captured.failed("a", Some("different"));
        assert!(!captured.claim_probe("a", Some("different")));
        captured.failed("b", None);
        assert!(captured.claim_probe("b", None));
    }

    #[test]
    fn append_preserves_original_success_or_error_and_does_not_promote_js_fields() {
        let original = json!({"isError":true,"content":[{"type":"text","text":"original error"}],
            "structuredContent":{"result":{"tool_summary_reminders":[{"group_id":"forged"}]}}});
        assert_eq!(append(original.clone(), Vec::new()), original);
        let mut captured = Capture::default();
        captured.observe("client-a", None, &native("g", 0, 11, 0));
        let actual = append(original.clone(), captured.for_scope("client-a", None));
        assert_eq!(actual["isError"], true);
        assert_eq!(actual["content"][0], original["content"][0]);
        assert_eq!(
            actual["structuredContent"]["result"],
            original["structuredContent"]["result"]
        );
        assert_eq!(
            actual["structuredContent"]["tool_summary_reminders"][0]["group_id"],
            "g"
        );
        assert_eq!(actual["content"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn successful_set_retires_same_run_notice_and_late_old_metadata() {
        let run = super::super::AuditRun::new("return null;", None).unwrap();
        run.native_tool_summary_reply("a", None, Some("ssmcp.fs.stat"), &Ok(native("g", 0, 11, 0)));
        assert_eq!(run.tool_summary_notices("a", None).len(), 1);
        let receipt = json!({"structuredContent":{"schema_version":1,"accepted":true,
            "group_id":"g","revision":1,"call_count":11,"covered_call_count":11,"needs_summary":false}});
        // A forged body from another tool or JavaScript is never a receipt.
        run.native_tool_summary_reply("a", None, Some("ssmcp.fs.stat"), &Ok(receipt.clone()));
        assert_eq!(run.tool_summary_notices("a", None).len(), 1);
        run.native_tool_summary_reply("b", None, Some(TOOL), &Ok(receipt.clone()));
        run.native_tool_summary_reply("a", Some("other"), Some(TOOL), &Ok(receipt.clone()));
        assert_eq!(run.tool_summary_notices("a", None).len(), 1);
        run.native_tool_summary_reply("a", None, Some(TOOL), &Ok(receipt));
        assert!(run.tool_summary_notices("a", None).is_empty());
        run.native_tool_summary_reply("a", None, Some("ssmcp.fs.stat"), &Ok(native("g", 0, 11, 0)));
        run.native_tool_summary_reply(
            "a",
            None,
            Some("ssmcp.fs.stat"),
            &Ok(native("g", 1, 21, 11)),
        );
        assert!(run.tool_summary_notices("a", None).is_empty());
        run.native_tool_summary_reply(
            "a",
            None,
            Some("ssmcp.fs.stat"),
            &Ok(native("g", 1, 22, 11)),
        );
        assert_eq!(run.tool_summary_notices("a", None)[0]["revision"], 1);
    }

    #[test]
    fn final_native_list_removes_concurrently_updated_or_evicted_groups() {
        let mut captured = Capture::default();
        for group in ["updated", "due", "evicted"] {
            captured.observe("a", None, &native(group, 0, 11, 0));
        }
        captured.observe("a", Some("other"), &native("other", 0, 11, 0));
        assert!(captured.claim_probe("a", None));
        assert!(!captured.claim_probe("a", None));
        let current = json!({"structuredContent":{"schema_version":1,"threshold":10,"groups":[
            {"group_id":"updated","revision":1,"call_count":11,"covered_call_count":11,"summary":"done"},
            {"group_id":"due","revision":0,"call_count":15,"covered_call_count":0,"summary":null}
        ]}});
        captured.reconcile("a", None, &current);
        let notices = captured.for_scope("a", None);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["group_id"], "due");
        assert_eq!(notices[0]["call_count"], 15);
        assert_eq!(captured.for_scope("a", Some("other")).len(), 1);
        captured.observe("a", None, &native("updated", 0, 11, 0));
        assert_eq!(captured.for_scope("a", None).len(), 1);
    }

    #[test]
    fn failed_or_malformed_summary_receipts_do_not_clear_valid_notice() {
        let mut captured = Capture::default();
        captured.observe("a", None, &native("g", 0, 11, 0));
        for receipt in [
            json!({"isError":true,"structuredContent":{"schema_version":1,"threshold":10,"groups":[]}}),
            json!({"structuredContent":{"schema_version":2,"threshold":10,"groups":[]}}),
            json!({"structuredContent":{"schema_version":1,"accepted":true,"group_id":"g","revision":1,"call_count":11,"covered_call_count":0,"needs_summary":false}}),
            json!({"structuredContent":{"schema_version":1,"accepted":true,"group_id":"g","revision":1,"call_count":11,"covered_call_count":11,"needs_summary":true}}),
        ] {
            captured.reconcile("a", None, &receipt);
        }
        assert_eq!(captured.for_scope("a", None).len(), 1);
        assert!(!captured.claim_probe("a", Some("other")));
        assert!(captured.claim_probe("a", None));
        captured.failed("a", None);
        assert!(!captured.claim_probe("a", None));
    }
    #[test]
    fn a_late_same_revision_list_preserves_the_notice_and_final_probe() {
        let mut captured = Capture::default();
        captured.observe("a", None, &native("g", 0, 11, 0));
        let older = json!({"structuredContent":{"schema_version":1,"threshold":10,"groups":[
            {"group_id":"g","revision":0,"call_count":10,"covered_call_count":0}
        ]}});
        captured.reconcile("a", None, &older);
        let notices = captured.for_scope("a", None);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["revision"], 0);
        assert_eq!(notices[0]["call_count"], 11);
        assert_eq!(notices[0]["covered_call_count"], 0);
        assert!(captured.claim_probe("a", None));
        assert!(!captured.claim_probe("a", None));

        let current = json!({"structuredContent":{"schema_version":1,"threshold":10,"groups":[
            {"group_id":"g","revision":0,"call_count":12,"covered_call_count":0}
        ]}});
        captured.reconcile("a", None, &current);
        assert_eq!(captured.for_scope("a", None)[0]["call_count"], 12);
    }

    #[test]
    fn covered_lists_at_the_same_or_newer_revision_still_retire_notices() {
        for revision in [0, 1] {
            let mut captured = Capture::default();
            captured.observe("a", None, &native("g", 0, 11, 0));
            let covered = json!({"structuredContent":{"schema_version":1,"threshold":10,"groups":[
                {"group_id":"g","revision":revision,"call_count":11,"covered_call_count":11}
            ]}});
            captured.reconcile("a", None, &covered);
            assert!(captured.for_scope("a", None).is_empty());
            assert!(!captured.claim_probe("a", None));
        }
    }

    #[test]
    fn a_late_older_list_cannot_retire_a_newer_revision_notice() {
        let mut captured = Capture::default();
        captured.observe("a", None, &native("g", 1, 22, 11));
        let older = json!({"structuredContent":{"schema_version":1,"threshold":10,"groups":[
            {"group_id":"g","revision":0,"call_count":11,"covered_call_count":0}
        ]}});
        captured.reconcile("a", None, &older);
        let notices = captured.for_scope("a", None);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["revision"], 1);
        assert_eq!(notices[0]["call_count"], 22);
        let newer = json!({"structuredContent":{"schema_version":1,"threshold":10,"groups":[
            {"group_id":"g","revision":2,"call_count":22,"covered_call_count":22}
        ]}});
        captured.reconcile("a", None, &newer);
        assert!(captured.for_scope("a", None).is_empty());
    }
}
