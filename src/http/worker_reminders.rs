//! Bounded coordinator hints from the controlled client's live worker pool.
use super::{
    CodeRunReport, EligibleUserResponse, Operation,
    coordination_policy::{self, Dreaming, Policy},
    response_authorization,
};
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

    fn quiet(&self) -> bool {
        self.should_remind()
            && self.working == 0
            && self.offline == 0
            && self.standby() == self.total
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

#[derive(Default)]
struct GuidanceCache {
    entries: HashMap<Scope, (Policy, Instant, Option<String>)>,
}

impl GuidanceCache {
    fn due(&self, scope: &Scope, policy: &Policy, now: Instant) -> bool {
        policy.enabled
            && self.entries.get(scope).is_none_or(|(old, emitted, _)| {
                old != policy || now.saturating_duration_since(*emitted) >= REPEAT_AFTER
            })
    }

    fn dreaming_due(&self, scope: &Scope, round: &Dreaming) -> bool {
        self.entries
            .get(scope)
            .is_none_or(|(_, _, emitted)| emitted.as_deref() != Some(&round.round_id))
    }

    fn emitted(&mut self, scope: Scope, policy: Policy, now: Instant, round_id: Option<String>) {
        // Ordinary wait reminders and temporary disabling do not rearm a round.
        let round_id = round_id.or_else(|| {
            self.entries
                .get(&scope)
                .and_then(|(_, _, previous)| previous.clone())
        });
        if !self.entries.contains_key(&scope) && self.entries.len() >= MAX_CACHE_ENTRIES {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, (_, at, _))| *at)
                .map(|(key, _)| key.clone());
            if let Some(oldest) = oldest {
                self.entries.remove(&oldest);
            }
        }
        self.entries.insert(scope, (policy, now, round_id));
    }
}

fn guidance_cache() -> &'static Mutex<GuidanceCache> {
    static CACHE: OnceLock<Mutex<GuidanceCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn guidance(
    control_client_id: &str,
    agent_id: &str,
    pool_client_id: Option<&str>,
    policy: &Policy,
    round: Option<&Dreaming>,
) -> Value {
    let mut clients = vec![control_client_id];
    if let Some(pool) = pool_client_id.filter(|pool| *pool != control_client_id) {
        clients.push(pool);
    }
    // Custom worker filters must never remove the operator's wake-up signal.
    let mut kinds = policy.kinds.clone();
    if !kinds
        .iter()
        .any(|kind| kind == "user.input_available" || kind == "user.*")
    {
        kinds.push("user.input_available".into());
    }
    let maintenance = round.map_or_else(String::new, |round| {
        format!(
            " Dreaming round {}: after authorized work is finished, allow at most {} ms for one {} tool/workflow improvement. Reuse this round ID and original receipts; cancellation, timeout or unknown never starts another round. Yield to new user input. Use a detached worktree, validate and retain a reviewable diff without overwriting dirty files.",
            round.round_id, round.budget_ms, round.repository
        )
    });
    json!({
        "schema_version":1,
        "client_id":control_client_id,
        "agent_id":agent_id,
        "condition":"when_no_independent_work",
        "policy":policy,
        "dreaming_round":round,
        "subscription_filter":{"client_ids":clients,"kinds":kinds},
        "await_target_type":"event",
        "message":format!(
            "After independent work is finished, subscribe to the listed events and use the public await event target in windows of at most {} ms. Resume the same wait_id while pending. Process ready events, explicitly acknowledge their subscription cursor and the await result cursor, then start the next wait. user.input_available only wakes the observer; retrieve the actual input through ui.wait. Inspect unknown outcomes using the original operation ID. Change this advice with events action=policy.{}",
            policy.window_ms, maintenance
        )
    })
}

fn input_pending(target: &EligibleUserResponse, response: &Value) -> bool {
    if response["structuredContent"]["user_messages"]
        .as_array()
        .is_some_and(|messages| !messages.is_empty())
    {
        return true;
    }
    let agent_id = target
        .binding
        .as_ref()
        .map(|binding| binding.agent_id.as_str());
    // This is a bounded observation only: no reserve, claim, acknowledgement,
    // subscription, or user-input text enters the advisory hint.
    target
        .session
        .steering
        .command(
            target.session.client_id(),
            Vec::new(),
            crate::wire::SteeringCommand::Snapshot {},
        )
        .map_or(true, |snapshot| {
            snapshot.messages.iter().any(|message| {
                matches!(
                    message.state,
                    crate::wire::SteeringState::Queued | crate::wire::SteeringState::Editing
                ) && (message.target == "next" || Some(message.target.as_str()) == agent_id)
            })
        })
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
    append_section(
        response,
        notices,
        budget,
        "worker_reminders",
        "WORKER AVAILABILITY",
    )
}

fn append_section(
    response: &Value,
    notices: &[Value],
    budget: usize,
    key: &str,
    heading: &str,
) -> Option<Value> {
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
    if structured.contains_key(key) {
        return None;
    }
    structured.insert(key.into(), json!(notices));
    let text = notices
        .iter()
        .filter_map(|notice| notice["message"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    root.entry("content")
        .or_insert_with(|| json!([]))
        .as_array_mut()?
        .push(json!({"type":"text","text":format!("{heading}\n{text}")}));
    let growth = appended
        .to_string()
        .len()
        .saturating_sub(response.to_string().len());
    (growth <= budget).then_some(appended)
}

/// Probe only existing coordinator targets after the JS worker has been released.
/// Failed pool probes suppress pool hints; authenticated coordinators can still
/// receive their configured control-session input waiting guidance.
pub(super) async fn append(
    report: &CodeRunReport,
    response: Value,
    eligible: &[EligibleUserResponse],
) -> Value {
    let budget = MAX_APPEND_BYTES.min(super::delivery_budget(&response));
    if budget < 512 {
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
            // A linked identifier is a hint, not permission to subscribe to it.
            // Keep a live authorized connection for the final synchronous check.
            let connection = tokio::time::timeout_at(
                deadline,
                report
                    .broker
                    .client(&report.user, &authorization, &pool.client_id),
            )
            .await
            .ok()
            .and_then(Result::ok);
            observed.push((index, pool, connection));
        }
    }

    // No await follows these final binding/authorization decisions. A failed or
    // oversized append does not consume the reminder's next delivery opportunity.
    let mut notices = Vec::new();
    let mut appended = None;
    for (index, pool, _) in &observed {
        let target = &eligible[*index];
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
        notices.push(notice(target.session.client_id(), &binding.agent_id, pool));
        let Some(next) = append_notices(&response, &notices, budget) else {
            notices.pop();
            continue;
        };
        cache.emitted(scope, pool.counts.clone(), now);
        appended = Some(next);
    }
    let with_workers = appended.unwrap_or_else(|| response.clone());
    let remaining = budget.saturating_sub(
        with_workers
            .to_string()
            .len()
            .saturating_sub(response.to_string().len()),
    );
    let mut hints = Vec::new();
    let mut with_guidance = None;
    for (index, target) in eligible.iter().enumerate() {
        if hints.len() == MAX_REMINDERS
            || report.run.time_observation_only(target.session.client_id())
            || !is_coordinator(report, target)
        {
            continue;
        }
        let Some(authorization) = response_authorization(report, target) else {
            continue;
        };
        let binding = target
            .binding
            .as_ref()
            .expect("verified coordinator binding");
        if input_pending(target, &response) {
            continue;
        }
        let pool = observed.iter().find_map(|(at, pool, connection)| {
            (*at == index)
                .then_some(connection.as_ref())
                .flatten()
                .filter(|session| session.is_alive() && authorization.permits(session))
                .filter(|_| pool.counts.connected && !pool.counts.closed)
                .map(|_| pool)
        });
        let Ok(policy) = coordination_policy::effective(
            &binding.owner,
            target.session.client_id(),
            &binding.agent_id,
            None,
        ) else {
            continue;
        };
        let scope = Scope {
            connection: Arc::as_ptr(&target.session) as usize,
            client_id: target.session.client_id().into(),
            owner: binding.owner.clone(),
            agent_id: binding.agent_id.clone(),
            pool_id: pool.map_or("", |pool| pool.client_id.as_str()).into(),
        };
        let now = Instant::now();
        let mut cache = guidance_cache()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let round = policy.dreaming.as_ref().filter(|round| {
            policy.enabled
                && response["isError"] != true
                && pool.is_some_and(|pool| pool.counts.quiet())
                && cache.dreaming_due(&scope, round)
        });
        if !cache.due(&scope, &policy, now) && round.is_none() {
            continue;
        }
        hints.push(guidance(
            target.session.client_id(),
            &binding.agent_id,
            pool.map(|pool| pool.client_id.as_str()),
            &policy,
            round,
        ));
        let Some(next) = append_section(
            &with_workers,
            &hints,
            remaining,
            "coordination_hints",
            "COORDINATOR WAIT POLICY",
        ) else {
            hints.pop();
            continue;
        };
        let round_id = round.map(|round| round.round_id.clone());
        cache.emitted(scope, policy, now, round_id);
        with_guidance = Some(next);
    }
    with_guidance.unwrap_or(with_workers)
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

    #[test]
    fn idle_wait_guidance_does_not_require_standby_workers_or_claim_main_is_idle() {
        let mut pool = pool_from_reply(&reply()).unwrap();
        pool.counts.idle = 0;
        pool.counts.working = 3;
        assert!(!pool.counts.should_remind());
        let policy = Policy::default();
        let hint = guidance("control", "main", Some(&pool.client_id), &policy, None);
        assert_eq!(hint["condition"], "when_no_independent_work");
        assert_eq!(
            hint["subscription_filter"]["client_ids"],
            json!(["control", pool.client_id])
        );
        assert_eq!(hint["await_target_type"], "event");
        assert!(hint.get("status").is_none());
        assert_eq!(
            hint["policy"]["window_ms"],
            crate::wire::MAX_OBSERVATION_WAIT_MS
        );
        assert!(hint["message"].as_str().unwrap().contains("same wait_id"));
        assert!(hint["message"].as_str().unwrap().contains("ui.wait"));
    }

    #[test]
    fn absent_or_unavailable_pool_leaves_a_control_only_event_subscription() {
        let policy = Policy::default();
        let hint = guidance("control", "main", None, &policy, None);
        assert_eq!(
            hint["subscription_filter"]["client_ids"],
            json!(["control"])
        );
        let same = guidance("control", "main", Some("control"), &policy, None);
        assert_eq!(
            same["subscription_filter"]["client_ids"],
            json!(["control"])
        );
        assert!(hint.to_string().find("token").is_none());
    }

    #[test]
    fn custom_wait_filters_preserve_the_user_input_wakeup() {
        let mut policy = Policy {
            kinds: vec!["worker.completed".into()],
            ..Policy::default()
        };
        let hint = guidance("control", "main", None, &policy, None);
        assert_eq!(
            hint["subscription_filter"]["kinds"],
            json!(["worker.completed", "user.input_available"])
        );
        assert_eq!(policy.kinds, ["worker.completed"]);
        policy.kinds = vec!["user.*".into(), "custom.finished".into()];
        let hint = guidance("control", "main", None, &policy, None);
        assert_eq!(hint["subscription_filter"]["kinds"], json!(policy.kinds));
        policy.kinds = (0..16)
            .map(|index| format!("custom.event{index}"))
            .collect();
        let hint = guidance("control", "main", None, &policy, None);
        assert_eq!(
            hint["subscription_filter"]["kinds"]
                .as_array()
                .unwrap()
                .len(),
            17
        );
        assert_eq!(policy.kinds.len(), 16);
    }

    #[test]
    fn dreaming_requires_a_quiet_observed_pool() {
        let initial = reply();
        assert!(pool_from_reply(&initial).unwrap().counts.quiet());
        for changes in [
            json!({"idle":2,"working":1}),
            json!({"idle":2,"offline":1,"registered_workers":2}),
            json!({"queued":1}),
            json!({"stopping":true}),
            json!({"connected":false}),
            json!({"desired_total":2}),
        ] {
            let mut changed = initial.clone();
            changed["structuredContent"]["value"]
                .as_object_mut()
                .unwrap()
                .extend(changes.as_object().unwrap().clone());
            assert!(!pool_from_reply(&changed).unwrap().counts.quiet());
        }
    }

    #[test]
    fn dreaming_rounds_are_not_rearmed_by_wait_reminders_or_disabling() {
        let mut cache = GuidanceCache::default();
        let key = scope("main");
        let now = Instant::now();
        let mut policy = Policy {
            dreaming: Some(Dreaming {
                round_id: "round-1".into(),
                repository: "project".into(),
                budget_ms: 1000,
            }),
            ..Policy::default()
        };
        let round = policy.dreaming.as_ref().unwrap();
        assert!(cache.dreaming_due(&key, round));
        cache.emitted(
            key.clone(),
            policy.clone(),
            now,
            Some(round.round_id.clone()),
        );
        assert!(cache.due(&key, &policy, now + REPEAT_AFTER));
        assert!(!cache.dreaming_due(&key, round));
        cache.emitted(key.clone(), policy.clone(), now + REPEAT_AFTER, None);
        assert!(!cache.dreaming_due(&key, round));
        policy.enabled = false;
        assert!(!cache.due(&key, &policy, now + REPEAT_AFTER));
        policy.enabled = true;
        policy.dreaming.as_mut().unwrap().budget_ms = 2000;
        assert!(!cache.dreaming_due(&key, policy.dreaming.as_ref().unwrap()));
        policy.dreaming.as_mut().unwrap().round_id = "round-2".into();
        assert!(cache.dreaming_due(&key, policy.dreaming.as_ref().unwrap()));
    }

    #[test]
    fn guidance_is_throttled_but_policy_edits_take_effect_immediately() {
        let mut cache = GuidanceCache::default();
        let key = scope("main");
        let now = Instant::now();
        let policy = Policy::default();
        assert!(cache.due(&key, &policy, now));
        cache.emitted(key.clone(), policy.clone(), now, None);
        assert!(!cache.due(&key, &policy, now + Duration::from_secs(29)));
        assert!(cache.due(&key, &policy, now + REPEAT_AFTER));
        let mut changed = policy.clone();
        changed.window_ms = 1000;
        assert!(cache.due(&key, &changed, now));
        cache.emitted(key.clone(), changed.clone(), now, None);
        changed.enabled = false;
        assert!(!cache.due(&key, &changed, now));
        assert!(cache.entries.contains_key(&key));
        changed.enabled = true;
        assert!(!cache.due(&key, &changed, now));
        assert!(cache.due(&key, &changed, now + REPEAT_AFTER));
    }

    #[test]
    fn guidance_cache_is_bounded_and_scoped_to_the_participant_connection() {
        let mut cache = GuidanceCache::default();
        let now = Instant::now();
        let policy = Policy::default();
        for index in 0..=MAX_CACHE_ENTRIES {
            cache.emitted(
                scope(&index.to_string()),
                policy.clone(),
                now + Duration::from_millis(index as u64),
                None,
            );
        }
        assert_eq!(cache.entries.len(), MAX_CACHE_ENTRIES);
        assert!(!cache.entries.contains_key(&scope("0")));
        let old = scope("1");
        assert!(!cache.due(&old, &policy, now));
        let mut reconnected = old.clone();
        reconnected.connection = 2;
        assert!(cache.due(&reconnected, &policy, now));
        let mut other_owner = old;
        other_owner.owner = "other".into();
        assert!(cache.due(&other_owner, &policy, now));
    }

    #[test]
    fn availability_and_wait_guidance_share_one_bounded_reply_budget() {
        let original =
            json!({"isError":false,"content":[],"structuredContent":{"result":{"ready":true}}});
        let pool = pool_from_reply(&reply()).unwrap();
        let with_workers = append_notices(
            &original,
            &[notice("control", "main", &pool)],
            MAX_APPEND_BYTES,
        )
        .unwrap();
        let remaining =
            MAX_APPEND_BYTES - (with_workers.to_string().len() - original.to_string().len());
        let hint = guidance(
            "control",
            "main",
            Some(&pool.client_id),
            &Policy::default(),
            None,
        );
        let combined = append_section(
            &with_workers,
            &[hint],
            remaining,
            "coordination_hints",
            "COORDINATOR WAIT POLICY",
        )
        .unwrap();
        assert_eq!(
            combined["structuredContent"]["result"],
            original["structuredContent"]["result"]
        );
        assert!(combined.to_string().len() - original.to_string().len() <= MAX_APPEND_BYTES);
        assert_eq!(
            combined["structuredContent"]["worker_reminders"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            combined["structuredContent"]["coordination_hints"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn hints_do_not_overwrite_existing_fields_or_exceed_small_delivery_budgets() {
        let original =
            json!({"structuredContent":{"coordination_hints":[{"message":"already included"}]}});
        let hint = guidance("control", "main", None, &Policy::default(), None);
        assert!(
            append_section(
                &original,
                std::slice::from_ref(&hint),
                MAX_APPEND_BYTES,
                "coordination_hints",
                "COORDINATOR WAIT POLICY"
            )
            .is_none()
        );
        assert!(
            append_section(
                &json!({}),
                &[hint],
                100,
                "coordination_hints",
                "COORDINATOR WAIT POLICY"
            )
            .is_none()
        );
        assert_eq!(
            original["structuredContent"]["coordination_hints"][0]["message"],
            "already included"
        );
    }
}
