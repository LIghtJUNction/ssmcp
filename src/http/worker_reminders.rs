//! Bounded coordinator hints from the controlled client's live worker pool.
use super::{CodeRunReport, EligibleUserResponse, Operation, response_authorization};
use crate::code_mode;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

const MAX_REMINDERS: usize = 4;
const MAX_PROBES: usize = 8;
const MAX_APPEND_BYTES: usize = 4096;
const MAX_CACHE_ENTRIES: usize = 256;
const REPEAT_AFTER: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Scope {
    connection: usize,
    client_id: String,
    owner: String,
    agent_id: String,
    pool_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Counts {
    total: u64,
    desired_total: u64,
    idle: u64,
    working: u64,
    offline: u64,
    retiring: u64,
    queued: u64,
    registered: u64,
    stopping: bool,
    connected: bool,
    closed: bool,
}

impl Counts {
    fn standby(&self) -> u64 {
        self.idle.min(
            self.registered
                .saturating_sub(self.working)
                .saturating_sub(self.retiring),
        )
    }

    fn should_remind(&self) -> bool {
        self.connected
            && !self.closed
            && !self.stopping
            && self.retiring == 0
            && self.desired_total == self.total
            && self.queued == 0
            && self.standby() != 0
    }
}

struct Pool {
    client_id: String,
    process_id: u64,
    counts: Counts,
}

fn bounded_count(value: &Value, name: &str) -> Option<u64> {
    value.get(name)?.as_u64().filter(|count| *count <= 128)
}

fn count_or(value: &Value, name: &str, default: u64) -> Option<u64> {
    match value.get(name) {
        Some(_) => bounded_count(value, name),
        None => Some(default),
    }
}

fn flag_or(value: &Value, name: &str, default: bool) -> Option<bool> {
    match value.get(name) {
        Some(value) => value.as_bool(),
        None => Some(default),
    }
}

fn pool_from_reply(reply: &Value) -> Option<Pool> {
    if reply["isError"] == true || reply["structuredContent"]["path"] != "/worker_pool" {
        return None;
    }
    let value = reply.get("structuredContent")?.get("value")?;
    let client_id = value.get("client_id")?.as_str()?;
    if client_id.len() != 64 || !client_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let process_id = value
        .get("process_id")?
        .as_u64()
        .filter(|pid| (1..=u64::from(u32::MAX)).contains(pid))?;
    let total = bounded_count(value, "total")?;
    let idle = bounded_count(value, "idle")?;
    let working = bounded_count(value, "working")?;
    let registered = bounded_count(value, "registered_workers")?;
    let retiring = count_or(value, "retiring", 0)?;
    let standby = idle.min(registered.saturating_sub(working).saturating_sub(retiring));
    let reported_offline = value.get("offline").is_some();
    let counts = Counts {
        total,
        desired_total: count_or(value, "desired_total", total)?,
        idle,
        working,
        offline: count_or(
            value,
            "offline",
            total.saturating_sub(working + standby + retiring),
        )?,
        retiring,
        queued: bounded_count(value, "queued")?,
        registered,
        stopping: flag_or(value, "stopping", false)?,
        connected: flag_or(value, "connected", true)?,
        closed: flag_or(value, "closed", false)?,
    };
    // Older snapshots without offline included unregistered capacity in idle.
    // Their derived offline count already contains that capacity, so only actual
    // standby workers participate in the legacy four-way sum.
    let counted_idle = if reported_offline { idle } else { standby };
    if counts.total == 0
        || registered > total
        || idle + working + retiring > total
        || counted_idle + working + counts.offline + retiring != total
    {
        return None;
    }
    Some(Pool {
        client_id: client_id.into(),
        process_id,
        counts,
    })
}

fn coordinator_role(role: &str) -> bool {
    ["main", "coordinator", "controller", "lead"]
        .iter()
        .any(|allowed| role.eq_ignore_ascii_case(allowed))
}

fn is_coordinator(report: &CodeRunReport, target: &EligibleUserResponse) -> bool {
    let Some(binding) = &target.binding else {
        return false;
    };
    if !target.session.chat_enabled()
        || report
            .run
            .binding(target.session.client_id())
            .is_none_or(|current| {
                current.owner != binding.owner
                    || current.agent_id != binding.agent_id
                    || current.token != binding.token
            })
    {
        return false;
    }
    let Ok(member) =
        target
            .session
            .collaboration
            .bind(&binding.owner, &binding.agent_id, &binding.token)
    else {
        return false;
    };
    serde_json::to_value(member)
        .ok()
        .and_then(|member| member["role"].as_str().map(coordinator_role))
        .unwrap_or(false)
}

#[derive(Default)]
struct Cache {
    entries: HashMap<Scope, (Counts, Instant)>,
}

impl Cache {
    fn due(&mut self, scope: &Scope, counts: &Counts, now: Instant) -> bool {
        if !counts.should_remind() {
            self.entries.remove(scope);
            return false;
        }
        self.entries.get(scope).is_none_or(|(old, emitted)| {
            old != counts || now.saturating_duration_since(*emitted) >= REPEAT_AFTER
        })
    }

    fn emitted(&mut self, scope: Scope, counts: Counts, now: Instant) {
        if !self.entries.contains_key(&scope) && self.entries.len() >= MAX_CACHE_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(scope, (counts, now));
    }
}

fn cache() -> &'static Mutex<Cache> {
    static CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn notice(control_client_id: &str, agent_id: &str, pool: &Pool) -> Value {
    let standby = pool.counts.standby();
    json!({
        "schema_version":1,
        "client_id":control_client_id,
        "agent_id":agent_id,
        "worker_client_id":pool.client_id,
        "process_id":pool.process_id,
        "standby":standby,
        "working":pool.counts.working,
        "total":pool.counts.total,
        "desired_total":pool.counts.desired_total,
        "retiring":pool.counts.retiring,
        "registered_workers":pool.counts.registered,
        "message":format!(
            "Worker pool {}: {standby} connected external worker(s) are standing by. Delegate suitable independent work with ssmcp.worker.submit and review worker.result. This notice does not start agents.",
            pool.client_id
        )
    })
}

fn append_notices(response: &Value, notices: &[Value], budget: usize) -> Option<Value> {
    if notices.is_empty() || notices.len() > MAX_REMINDERS {
        return None;
    }
    let mut appended = response.clone();
    let root = appended.as_object_mut()?;
    if root
        .get("structuredContent")
        .is_some_and(|value| !value.is_object())
        || root.get("content").is_some_and(|value| !value.is_array())
    {
        return None;
    }
    let structured = root
        .entry("structuredContent")
        .or_insert_with(|| json!({}))
        .as_object_mut()?;
    if structured.contains_key("worker_reminders") {
        return None;
    }
    structured.insert("worker_reminders".into(), json!(notices));
    let text = notices
        .iter()
        .filter_map(|notice| notice["message"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    root.entry("content")
        .or_insert_with(|| json!([]))
        .as_array_mut()?
        .push(json!({"type":"text","text":format!("WORKER AVAILABILITY\n{text}")}));
    let growth = appended
        .to_string()
        .len()
        .saturating_sub(response.to_string().len());
    (growth <= budget).then_some(appended)
}

/// Probe only existing coordinator targets after the JS worker has been released.
/// Native failures and unsupported projections leave the original reply intact.
pub(super) async fn append(
    report: &CodeRunReport,
    response: Value,
    eligible: &[EligibleUserResponse],
) -> Value {
    let budget = MAX_APPEND_BYTES.min(super::delivery_budget(&response));
    if budget < 512
        || response
            .pointer("/structuredContent/worker_reminders")
            .is_some()
    {
        return response;
    }
    let deadline = tokio::time::Instant::now() + Duration::from_millis(750);
    let mut observed = Vec::new();
    let mut probes = 0;
    for (index, target) in eligible.iter().enumerate() {
        if probes >= MAX_PROBES || tokio::time::Instant::now() >= deadline {
            break;
        }
        if report.run.time_observation_only(target.session.client_id())
            || !is_coordinator(report, target)
        {
            continue;
        }
        let Some(authorization) = response_authorization(report, target) else {
            continue;
        };
        probes += 1;
        let result = tokio::time::timeout_at(
            deadline,
            code_mode::AUDIT_RUN.scope(
                report.run.clone(),
                target.session.call_with_authorization(
                    Operation::CallTool {
                        name: "ssmcp.ui.get".into(),
                        arguments: json!({"path":"/worker_pool"}),
                    },
                    None,
                    &authorization,
                ),
            ),
        )
        .await;
        if let Ok(Ok(reply)) = result
            && let Some(pool) = pool_from_reply(&reply)
        {
            observed.push((index, pool));
        }
    }

    // No await follows these final binding/authorization decisions. A failed or
    // oversized append does not consume the reminder's next delivery opportunity.
    let mut notices = Vec::new();
    let mut appended = None;
    for (index, pool) in observed {
        let target = &eligible[index];
        if response_authorization(report, target).is_none() || !is_coordinator(report, target) {
            continue;
        }
        let binding = target
            .binding
            .as_ref()
            .expect("verified coordinator binding");
        let scope = Scope {
            connection: Arc::as_ptr(&target.session) as usize,
            client_id: target.session.client_id().into(),
            owner: binding.owner.clone(),
            agent_id: binding.agent_id.clone(),
            pool_id: pool.client_id.clone(),
        };
        let now = Instant::now();
        let mut cache = cache().lock().unwrap_or_else(|error| error.into_inner());
        if !cache.due(&scope, &pool.counts, now) || notices.len() == MAX_REMINDERS {
            continue;
        }
        notices.push(notice(target.session.client_id(), &binding.agent_id, &pool));
        let Some(next) = append_notices(&response, &notices, budget) else {
            notices.pop();
            continue;
        };
        cache.emitted(scope, pool.counts, now);
        appended = Some(next);
    }
    appended.unwrap_or(response)
}

#[cfg(test)]
mod session_tests;

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> Value {
        json!({
            "isError":false,
            "structuredContent":{
                "schema_version":1,"path":"/worker_pool",
                "value":{
                    "client_id":"a".repeat(64),"process_id":123,
                    "total":3,"idle":3,"working":0,"offline":0,"queued":0,
                    "registered_workers":3
                }
            }
        })
    }

    fn scope(agent: &str) -> Scope {
        Scope {
            connection: 1,
            client_id: "control".into(),
            owner: "owner".into(),
            agent_id: agent.into(),
            pool_id: "a".repeat(64),
        }
    }

    #[test]
    fn idle_capacity_without_connected_workers_does_not_remind() {
        let mut reply = reply();
        reply["structuredContent"]["value"]["registered_workers"] = json!(0);
        let pool = pool_from_reply(&reply).unwrap();
        assert_eq!(pool.counts.standby(), 0);
        assert!(!pool.counts.should_remind());
    }

    #[test]
    fn fully_staffed_pool_can_have_standby_workers() {
        let pool = pool_from_reply(&reply()).unwrap();
        assert_eq!(pool.counts.standby(), 3);
        assert!(pool.counts.should_remind());
    }

    #[test]
    fn more_registered_workers_than_stations_is_invalid_native_state() {
        let mut input = reply();
        input["structuredContent"]["value"]["registered_workers"] = json!(4);
        assert!(pool_from_reply(&input).is_none());
    }

    #[test]
    fn legacy_snapshot_defaults_only_missing_resize_fields() {
        let pool = pool_from_reply(&reply()).unwrap();
        assert_eq!(pool.counts.desired_total, pool.counts.total);
        assert_eq!(pool.counts.retiring, 0);
        assert!(pool.counts.connected);
        assert!(!pool.counts.closed);
        assert!(pool.counts.should_remind());

        let mut input = reply();
        input["structuredContent"]["value"]
            .as_object_mut()
            .unwrap()
            .remove("registered_workers");
        assert!(pool_from_reply(&input).is_none());
    }

    #[test]
    fn legacy_offline_fallback_counts_unregistered_idle_capacity_once() {
        let mut input = reply();
        input["structuredContent"]["value"]["registered_workers"] = json!(1);
        input["structuredContent"]["value"]
            .as_object_mut()
            .unwrap()
            .remove("offline");
        let pool = pool_from_reply(&input).unwrap();
        // Raw legacy idle still includes two unfilled stations; only the one
        // registered standby worker participates in the compatible sum.
        assert_eq!(pool.counts.idle, 3);
        assert_eq!(pool.counts.standby(), 1);
        assert_eq!(pool.counts.offline, 2);
        assert_eq!(
            pool.counts.standby()
                + pool.counts.working
                + pool.counts.offline
                + pool.counts.retiring,
            pool.counts.total
        );
        assert!(pool.counts.should_remind());

        input["structuredContent"]["value"]["working"] = json!(1);
        assert!(pool_from_reply(&input).is_none());
    }

    #[test]
    fn reported_offline_is_preserved_instead_of_inferred_from_registration() {
        let mut input = reply();
        input["structuredContent"]["value"]["registered_workers"] = json!(1);
        let pool = pool_from_reply(&input).unwrap();
        assert_eq!(pool.counts.standby(), 1);
        assert_eq!(pool.counts.offline, 0);
        assert_eq!(
            pool.counts.idle + pool.counts.working + pool.counts.offline + pool.counts.retiring,
            pool.counts.total
        );
    }

    #[test]
    fn busy_stations_still_count_as_working_during_shrink() {
        let mut input = reply();
        input["structuredContent"]["value"]["idle"] = json!(1);
        input["structuredContent"]["value"]["working"] = json!(2);
        input["structuredContent"]["value"]["desired_total"] = json!(1);
        input["structuredContent"]["value"]["retiring"] = json!(0);
        let pool = pool_from_reply(&input).unwrap();
        assert_eq!(pool.counts.total, 3);
        assert_eq!(pool.counts.working, 2);
        assert_eq!(pool.counts.retiring, 0);
        assert_eq!(pool.counts.standby(), 1);
        assert!(!pool.counts.should_remind());

        // Counting a busy station again as retiring violates the snapshot sum.
        input["structuredContent"]["value"]["retiring"] = json!(1);
        assert!(pool_from_reply(&input).is_none());
    }

    #[test]
    fn retiring_stations_suppress_hints_even_when_the_target_matches() {
        let mut input = reply();
        input["structuredContent"]["value"]["idle"] = json!(2);
        input["structuredContent"]["value"]["retiring"] = json!(1);
        input["structuredContent"]["value"]["desired_total"] = json!(3);
        let pool = pool_from_reply(&input).unwrap();
        assert_eq!(pool.counts.standby(), 2);
        assert_eq!(pool.counts.offline, 0);
        assert!(!pool.counts.should_remind());
    }

    #[test]
    fn legacy_offline_fallback_excludes_retiring_workers_from_standby() {
        let mut input = reply();
        let value = &mut input["structuredContent"]["value"];
        value["total"] = json!(4);
        value["desired_total"] = json!(3);
        value["idle"] = json!(2);
        value["working"] = json!(1);
        value["retiring"] = json!(1);
        value["registered_workers"] = json!(2);
        value.as_object_mut().unwrap().remove("offline");
        let pool = pool_from_reply(&input).unwrap();
        assert_eq!(pool.counts.standby(), 0);
        assert_eq!(pool.counts.offline, 2);
        assert!(!pool.counts.should_remind());
    }

    #[test]
    fn unmatched_capacity_target_suppresses_hints_before_retirement_starts() {
        for desired in [0, 2, 4] {
            let mut input = reply();
            input["structuredContent"]["value"]["desired_total"] = json!(desired);
            let pool = pool_from_reply(&input).unwrap();
            assert_eq!(pool.counts.retiring, 0);
            assert_eq!(pool.counts.standby(), 3);
            assert!(!pool.counts.should_remind());
        }
    }

    #[test]
    fn four_way_snapshot_counts_must_equal_the_actual_total() {
        let mut input = reply();
        for (field, count) in [
            ("total", 4),
            ("desired_total", 4),
            ("idle", 1),
            ("working", 1),
            ("offline", 1),
            ("retiring", 1),
        ] {
            input["structuredContent"]["value"][field] = json!(count);
        }
        let pool = pool_from_reply(&input).unwrap();
        assert_eq!(pool.counts.total, 4);
        assert_eq!(pool.counts.retiring, 1);
        assert_eq!(pool.counts.offline, 1);
        input["structuredContent"]["value"]["offline"] = json!(2);
        assert!(pool_from_reply(&input).is_none());
        input["structuredContent"]["value"]["offline"] = json!(0);
        assert!(pool_from_reply(&input).is_none());
    }

    #[test]
    fn disconnected_or_closed_pools_do_not_remind() {
        let mut input = reply();
        input["structuredContent"]["value"]["connected"] = json!(false);
        let pool = pool_from_reply(&input).unwrap();
        assert_eq!(pool.counts.standby(), 3);
        assert!(!pool.counts.should_remind());

        input["structuredContent"]["value"]["connected"] = json!(true);
        input["structuredContent"]["value"]["closed"] = json!(true);
        assert!(!pool_from_reply(&input).unwrap().counts.should_remind());
        input["structuredContent"]["value"]["closed"] = json!(false);
        assert!(pool_from_reply(&input).unwrap().counts.should_remind());
    }

    #[test]
    fn present_optional_fields_must_have_the_declared_types() {
        for (field, value) in [
            ("retiring", json!(null)),
            ("retiring", json!("0")),
            ("desired_total", json!(null)),
            ("desired_total", json!(true)),
            ("desired_total", json!(129)),
            ("offline", json!(null)),
            ("connected", json!("true")),
            ("closed", json!(0)),
            ("stopping", json!(null)),
        ] {
            let mut input = reply();
            input["structuredContent"]["value"][field] = value;
            assert!(pool_from_reply(&input).is_none(), "accepted {field}");
        }
    }

    #[test]
    fn standby_counts_only_connected_workers_without_an_active_task() {
        let mut counts = pool_from_reply(&reply()).unwrap().counts;
        counts.idle = 2;
        counts.working = 1;
        counts.registered = 2;
        assert_eq!(counts.standby(), 1);
        counts.registered = 0;
        assert_eq!(counts.standby(), 0);
    }

    #[test]
    fn busy_queued_and_stopping_pools_do_not_remind() {
        let mut counts = pool_from_reply(&reply()).unwrap().counts;
        counts.idle = 0;
        counts.working = 3;
        assert!(!counts.should_remind());
        counts.idle = 3;
        counts.working = 0;
        counts.queued = 1;
        assert!(!counts.should_remind());
        counts.queued = 0;
        counts.stopping = true;
        assert!(!counts.should_remind());
    }

    #[test]
    fn malformed_or_incomplete_native_state_is_not_zero_capacity() {
        for (field, value) in [
            ("total", json!(129)),
            ("total", json!(0)),
            ("idle", json!(-1)),
            ("working", json!(1.5)),
            ("queued", json!("0")),
            ("registered_workers", json!(null)),
            ("process_id", json!(0)),
            ("client_id", json!("unverified-client")),
        ] {
            let mut input = reply();
            input["structuredContent"]["value"][field] = value;
            assert!(pool_from_reply(&input).is_none(), "accepted {field}");
        }
        let mut input = reply();
        input["structuredContent"]["value"]["offline"] = json!(1);
        assert!(pool_from_reply(&input).is_none());
        input = reply();
        input["isError"] = json!(true);
        assert!(pool_from_reply(&input).is_none());
        input = reply();
        input["structuredContent"]["path"] = json!("/other");
        assert!(pool_from_reply(&input).is_none());
    }

    #[test]
    fn unchanged_counts_are_throttled_and_state_changes_rearm_immediately() {
        let mut cache = Cache::default();
        let key = scope("main");
        let counts = pool_from_reply(&reply()).unwrap().counts;
        let now = Instant::now();
        assert!(cache.due(&key, &counts, now));
        cache.emitted(key.clone(), counts.clone(), now);
        assert!(!cache.due(&key, &counts, now + Duration::from_secs(29)));
        assert!(cache.due(&key, &counts, now + REPEAT_AFTER));
        let mut changed = counts.clone();
        changed.idle = 2;
        changed.working = 1;
        assert!(cache.due(&key, &changed, now));
        cache.emitted(key.clone(), changed.clone(), now);
        changed.queued = 1;
        assert!(!cache.due(&key, &changed, now));
        assert!(cache.due(&key, &counts, now));
    }

    #[test]
    fn participant_owner_pool_and_connection_do_not_share_throttling() {
        let mut cache = Cache::default();
        let key = scope("main");
        let counts = pool_from_reply(&reply()).unwrap().counts;
        let now = Instant::now();
        cache.emitted(key.clone(), counts.clone(), now);
        let mut variants = vec![scope("another-main")];
        let mut other = key.clone();
        other.owner = "another-owner".into();
        variants.push(other);
        let mut other = key.clone();
        other.pool_id = "b".repeat(64);
        variants.push(other);
        let mut other = key.clone();
        other.connection = 2;
        variants.push(other);
        for other in variants {
            assert!(cache.due(&other, &counts, now));
        }
        assert!(!cache.due(&key, &counts, now));
    }

    #[test]
    fn cache_stays_bounded_across_retired_connections() {
        let mut cache = Cache::default();
        let counts = pool_from_reply(&reply()).unwrap().counts;
        let now = Instant::now();
        for index in 0..=MAX_CACHE_ENTRIES {
            cache.emitted(
                scope(&format!("main-{index}")),
                counts.clone(),
                now + Duration::from_millis(index as u64),
            );
        }
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        assert!(!cache.entries.contains_key(&scope("main-0")));
        assert!(
            cache
                .entries
                .contains_key(&scope(&format!("main-{MAX_CACHE_ENTRIES}")))
        );
    }

    #[test]
    fn notices_preserve_original_results_and_respect_the_append_budget() {
        let pool = pool_from_reply(&reply()).unwrap();
        let notice = notice("control", "main", &pool);
        let original = json!({
            "isError":true,
            "content":[{"type":"text","text":"original result"}],
            "structuredContent":{"user_messages":[{"text":"original input"}]}
        });
        assert!(append_notices(&original, std::slice::from_ref(&notice), 10).is_none());
        let appended = append_notices(&original, &[notice], MAX_APPEND_BYTES).unwrap();
        assert_eq!(appended["isError"], original["isError"]);
        assert_eq!(appended["content"][0], original["content"][0]);
        assert_eq!(
            appended["structuredContent"]["user_messages"],
            original["structuredContent"]["user_messages"]
        );
        assert_eq!(
            appended["structuredContent"]["worker_reminders"][0]["standby"],
            3
        );
        assert!(
            appended["content"][1]["text"]
                .as_str()
                .unwrap()
                .starts_with("WORKER AVAILABILITY")
        );
        assert!(appended.to_string().len() - original.to_string().len() <= MAX_APPEND_BYTES);
    }

    #[test]
    fn only_coordinator_roles_receive_availability_hints() {
        for role in ["main", "coordinator", "controller", "lead", "MAIN"] {
            assert!(coordinator_role(role));
        }
        for role in ["worker", "remote-worker", "ssmcp-worker", "", "main-worker"] {
            assert!(!coordinator_role(role));
        }
    }
}
