use serde_json::Value;

const NO_CONTROL_SESSION: &str = "Connect the control session before creating an invitation.";
const NO_WORKER_POOL: &str = "No linked worker pool is available.";
const FULLY_STAFFED: &str = "Worker stations are fully staffed.";
const RESIZING: &str =
    "Worker stations are resizing or retiring; wait before inviting another worker.";

pub(super) fn current_prompt(arguments: &str) -> Result<String, String> {
    let snapshot = super::steering::snapshot();
    let control_id = active_control_id(&snapshot)?;
    let cwd = std::env::current_dir()
        .map_err(|error| format!("Cannot read the current working directory: {error}"))?;
    let pool = super::worker_window::linked_snapshot();
    render(arguments, control_id, &cwd.to_string_lossy(), pool.as_ref())
}

fn active_control_id(snapshot: &Value) -> Result<&str, String> {
    if snapshot.get("connected").and_then(Value::as_bool) != Some(true) {
        return Err(NO_CONTROL_SESSION.into());
    }
    snapshot
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .ok_or_else(|| NO_CONTROL_SESSION.into())
}

#[derive(Clone, Copy)]
enum Role {
    Coordinator,
    Worker,
}

impl Role {
    fn parse(arguments: &str) -> Result<Self, String> {
        match arguments.trim() {
            "" => Ok(Self::Coordinator),
            "worker" => Ok(Self::Worker),
            _ => Err("Usage: /invite [worker].".into()),
        }
    }
}

struct Pool<'a> {
    client_id: &'a str,
    process_id: u64,
    total: u64,
    desired_total: u64,
    registered: u64,
    working: u64,
    standby: u64,
    offline: u64,
    retiring: u64,
    queued: u64,
}

impl<'a> Pool<'a> {
    fn from_snapshot(value: &'a Value) -> Result<Self, String> {
        let flag = |key: &str, default: bool| {
            value.get(key).map_or(Ok(default), |value| {
                value
                    .as_bool()
                    .ok_or_else(|| format!("The linked worker pool has an invalid {key} flag."))
            })
        };
        if !flag("connected", true)? || flag("closed", false)? || flag("stopping", false)? {
            return Err("The worker pool is stopping or disconnected.".into());
        }
        let client_id = value
            .get("client_id")
            .and_then(Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .ok_or_else(|| "The linked worker pool has no active client ID.".to_owned())?;
        let count = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("The linked worker pool is missing its {key} value."))
        };
        let total = count("total")?;
        let desired_total = if value.get("desired_total").is_some() {
            count("desired_total")?
        } else {
            total
        };
        let retiring = if value.get("retiring").is_some() {
            count("retiring")?
        } else {
            0
        };
        let registered = count("registered_workers")?;
        let working = count("working")?;
        let idle = count("idle")?;
        let standby = idle.min(registered.saturating_sub(working).saturating_sub(retiring));
        let occupied = idle
            .checked_add(working)
            .and_then(|value| value.checked_add(retiring));
        if occupied.is_none_or(|occupied| occupied > total) {
            return Err("The linked worker pool has inconsistent station counts.".into());
        }
        let offline = if value.get("offline").is_some() {
            let offline = count("offline")?;
            if occupied.and_then(|occupied| occupied.checked_add(offline)) != Some(total) {
                return Err("The linked worker pool has inconsistent station counts.".into());
            }
            offline
        } else {
            // Older snapshots counted unregistered capacity as idle.
            total.saturating_sub(working.saturating_add(standby).saturating_add(retiring))
        };
        Ok(Self {
            client_id,
            process_id: count("process_id")?,
            total,
            desired_total,
            registered,
            working,
            standby,
            offline,
            retiring,
            queued: count("queued")?,
        })
    }

    fn description(&self) -> String {
        format!(
            "Worker pool: `{}` (PID {}). Stations: {} (target {}); registered workers: {}; working: {}; standby: {}; offline: {}; retiring: {}; queued: {}.\n",
            self.client_id,
            self.process_id,
            self.total,
            self.desired_total,
            self.registered,
            self.working,
            self.standby,
            self.offline,
            self.retiring,
            self.queued,
        )
    }
}

fn render(role: &str, control_id: &str, cwd: &str, pool: Option<&Value>) -> Result<String, String> {
    let role = Role::parse(role)?;
    if control_id.trim().is_empty() {
        return Err(NO_CONTROL_SESSION.into());
    }
    let pool = pool.map(Pool::from_snapshot).transpose()?;
    if matches!(role, Role::Worker) {
        let pool = pool.as_ref().ok_or_else(|| NO_WORKER_POOL.to_owned())?;
        if pool.retiring != 0 || pool.desired_total != pool.total {
            return Err(RESIZING.into());
        }
        if pool.registered >= pool.desired_total {
            return Err(FULLY_STAFFED.into());
        }
    }

    let mut prompt = format!("@ssmcp\nControl session: `{control_id}`.\nWorkspace: {cwd:?}.\n");
    match pool.as_ref() {
        Some(pool) => prompt.push_str(&pool.description()),
        None => prompt.push_str("Worker pool: none linked.\n"),
    }
    prompt.push_str("Read `ssmcp://guide`, `AGENTS.md`, `docs/workers.md`; discover schemas. First bind/reconnect: `await skills.view(clientId,{})` (native `ssmcp.skills.view`).\n");
    prompt.push_str(match role {
        Role::Coordinator => {
            "Coordinator: privately `collab.join` (role main) and `collab.bind` control. For a linked pool, bind there separately. Delegate to standby workers via `ssmcp.worker.attach`, `ssmcp.worker.submit`, `ssmcp.worker.tasks`, `ssmcp.worker.result` while working. Use your verified finish receipt for yourself; ask a worker-specific human question for a worker."
        }
        Role::Worker => {
            "External worker: privately `collab.join` (role worker) and `collab.bind` pool. `ssmcp.worker.wait`: preserve each claim unchanged; retain pending request_id. `ssmcp.worker.complete`, then wait again using a new request ID. For operator-ordered retirement, complete any current claim; acknowledge its receipt via `ssmcp.worker.release`. For voluntary exit verify the coordinator's human decision reference. Never manage the queue or consume control-session input."
        }
    });
    prompt.push('\n');
    prompt.push_str(crate::collab::DREAMING_POLICY);
    prompt.push_str("\nReconnect: refresh `list` and schemas; verify authorized workspace/control/pool; privately rejoin/bind. Reconcile claims, commits and outcomes before assigning. Preserve `unknown`; never replay or restart models. Follow `ssmcp://recovery`: 1/2/4/8s; prefer verified authorized 007, guarded `ssmcp.007.recover` with matching IDs. Respect stop/revocation; keep blocked work resumable.");
    Ok(prompt)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CONTROL_ID: &str = "control-0123456789abcdef";
    const POOL_ID: &str = "pool-fedcba9876543210";

    fn pool() -> Value {
        json!({
            "client_id": POOL_ID,
            "process_id": 31415,
            "total": 3,
            "desired_total": 3,
            "idle": 1,
            "working": 0,
            "offline": 2,
            "retiring": 0,
            "queued": 2,
            "registered_workers": 1,
            "token": "DO_NOT_DISCLOSE",
            "credentials": {"token": "ALSO_PRIVATE"},
        })
    }

    #[test]
    fn invalid_roles_are_rejected() {
        for role in ["coordinator", "workers", "Worker", "worker extra"] {
            assert_eq!(
                render(role, CONTROL_ID, "/project", Some(&pool())).unwrap_err(),
                "Usage: /invite [worker]."
            );
        }
    }

    #[test]
    fn invitations_require_a_connected_control_session() {
        for snapshot in [
            json!({"connected": false, "session_id": CONTROL_ID}),
            json!({"connected": true}),
            json!({"connected": true, "session_id": " "}),
        ] {
            assert_eq!(
                active_control_id(&snapshot).unwrap_err(),
                NO_CONTROL_SESSION
            );
        }
        let snapshot = json!({"connected": true, "session_id": CONTROL_ID});
        assert_eq!(active_control_id(&snapshot).unwrap(), CONTROL_ID);
        assert_eq!(
            render("", "", "/project", None).unwrap_err(),
            NO_CONTROL_SESSION
        );
    }

    #[test]
    fn only_coordinator_can_be_invited_without_a_linked_pool() {
        let prompt = render("", CONTROL_ID, "/project", None).unwrap();
        assert!(prompt.starts_with("@ssmcp\n"));
        assert!(prompt.contains(CONTROL_ID));
        assert!(prompt.contains("Worker pool: none linked."));
        assert!(prompt.contains("For a linked pool"));
        assert_eq!(
            render("worker", CONTROL_ID, "/project", None).unwrap_err(),
            NO_WORKER_POOL
        );
    }

    #[test]
    fn fully_staffed_idle_pool_rejects_another_worker() {
        let mut pool = pool();
        pool["registered_workers"] = json!(3);
        pool["idle"] = json!(3);
        pool["offline"] = json!(0);
        assert_eq!(
            render("worker", CONTROL_ID, "/project", Some(&pool)).unwrap_err(),
            FULLY_STAFFED
        );
        let prompt = render("", CONTROL_ID, "/project", Some(&pool)).unwrap();
        assert!(prompt.contains("standby: 3; offline: 0; retiring: 0"));
    }

    #[test]
    fn idle_unfilled_slots_are_not_described_as_standby_workers() {
        let prompt = render("worker", CONTROL_ID, "/project", Some(&pool())).unwrap();
        assert!(prompt.contains("Stations: 3 (target 3); registered workers: 1; working: 0; standby: 1; offline: 2; retiring: 0; queued: 2."));
        assert!(prompt.contains(CONTROL_ID));
        assert!(prompt.contains(POOL_ID));
        assert!(prompt.contains("PID 31415"));
    }

    #[test]
    fn busy_workers_are_excluded_from_standby_counts() {
        let mut pool = pool();
        pool["total"] = json!(5);
        pool["desired_total"] = json!(5);
        pool["registered_workers"] = json!(3);
        pool["working"] = json!(2);
        let prompt = render("worker", CONTROL_ID, "/project", Some(&pool)).unwrap();
        assert!(prompt.contains("working: 2; standby: 1; offline: 2"));
    }

    #[test]
    fn counts_do_not_underflow_during_worker_departure() {
        let mut pool = pool();
        pool["working"] = json!(2);
        pool["offline"] = json!(0);
        let prompt = render("", CONTROL_ID, "/project", Some(&pool)).unwrap();
        assert!(prompt.contains("working: 2; standby: 0; offline: 0"));
    }

    #[test]
    fn prompts_are_bounded_and_do_not_include_credentials() {
        for role in ["", "worker"] {
            let prompt = render(role, CONTROL_ID, "/project with spaces", Some(&pool())).unwrap();
            assert!(prompt.split_whitespace().count() <= 200);
            assert!(prompt.contains("ssmcp://guide"));
            assert!(prompt.contains("AGENTS.md"));
            assert!(prompt.contains("docs/workers.md"));
            assert!(prompt.contains("collab.join"));
            assert!(prompt.contains("collab.bind"));
            assert!(prompt.contains("Workspace: \"/project with spaces\"."));
            for private in ["token", "DO_NOT_DISCLOSE", "ALSO_PRIVATE"] {
                assert!(!prompt.contains(private));
            }
        }
    }

    #[test]
    fn invitations_attempt_reconnect_and_reconcile_before_new_work() {
        for role in ["", "worker"] {
            let prompt = render(role, CONTROL_ID, "/project", Some(&pool())).unwrap();
            for instruction in [
                "Reconnect: refresh `list` and schemas",
                "verify authorized workspace/control/pool",
                "privately rejoin/bind",
                "Reconcile claims, commits and outcomes before assigning",
                "Preserve `unknown`; never replay or restart models",
                "1/2/4/8s",
                "guarded `ssmcp.007.recover` with matching IDs",
                "idle_after_ms (default 5 minutes)",
                "resume working immediately; reset timing",
                "30 continuous Dreaming minutes",
                "Keep one pending request",
                "Human No: wait another full 30 minutes",
                "matching explicit Human Yes",
            ] {
                assert!(prompt.contains(instruction), "{role:?}: {instruction}");
            }
            for overview in ["await skills.view(clientId,{})", "ssmcp.skills.view"] {
                assert_eq!(prompt.matches(overview).count(), 1, "{role:?}: {overview}");
            }
        }
    }

    #[test]
    fn worker_prompt_preserves_claims_and_returns_to_waiting() {
        let prompt = render("worker", CONTROL_ID, "/project", Some(&pool())).unwrap();
        assert!(prompt.contains("preserve each claim unchanged"));
        assert!(prompt.contains("ssmcp.worker.wait"));
        assert!(prompt.contains("ssmcp.worker.complete"));
        assert!(prompt.contains("then wait again"));
        assert!(prompt.contains("then wait again using a new request ID"));
        assert!(prompt.contains("complete any current claim"));
        assert!(prompt.contains("ssmcp.worker.release"));
        assert!(prompt.contains("operator-ordered retirement"));
        assert!(prompt.contains(crate::collab::DREAMING_POLICY));
        assert!(prompt.contains("Never manage the queue or consume control-session input."));
        assert!(!prompt.contains("ssmcp.worker.attach"));
        assert!(!prompt.contains("ssmcp.worker.submit"));
    }

    #[test]
    fn coordinator_prompt_uses_authenticated_pool_management() {
        let prompt = render("", CONTROL_ID, "/project", Some(&pool())).unwrap();
        for tool in ["attach", "submit", "tasks", "result"] {
            assert!(prompt.contains(&format!("ssmcp.worker.{tool}")));
        }
        assert!(prompt.contains("Delegate to standby workers"));
        assert!(prompt.contains(crate::collab::DREAMING_POLICY));
    }

    #[test]
    fn incomplete_pool_state_is_not_presented_as_a_real_station_count() {
        let mut pool = pool();
        pool.as_object_mut().unwrap().remove("registered_workers");
        assert!(render("worker", CONTROL_ID, "/project", Some(&pool)).is_err());
        pool["registered_workers"] = json!(1);
        pool["client_id"] = json!("");
        assert!(render("worker", CONTROL_ID, "/project", Some(&pool)).is_err());
    }

    #[test]
    fn legacy_snapshots_default_target_and_retiring_without_overriding_offline() {
        let mut pool = pool();
        let object = pool.as_object_mut().unwrap();
        object.remove("desired_total");
        object.remove("retiring");
        pool["idle"] = json!(3);
        pool["offline"] = json!(0);
        let prompt = render("worker", CONTROL_ID, "/project", Some(&pool)).unwrap();
        assert!(prompt.contains("Stations: 3 (target 3)"));
        assert!(prompt.contains("standby: 1; offline: 0; retiring: 0"));
    }

    #[test]
    fn old_capacity_only_snapshots_derive_offline_from_connected_workers() {
        let mut pool = pool();
        let object = pool.as_object_mut().unwrap();
        object.remove("desired_total");
        object.remove("retiring");
        object.remove("offline");
        pool["idle"] = json!(3);
        let prompt = render("worker", CONTROL_ID, "/project", Some(&pool)).unwrap();
        assert!(prompt.contains("standby: 1; offline: 2; retiring: 0"));
    }

    #[test]
    fn retiring_stations_are_reported_separately_and_block_recruitment() {
        let mut pool = pool();
        pool["desired_total"] = json!(2);
        pool["idle"] = json!(0);
        pool["working"] = json!(1);
        pool["offline"] = json!(1);
        pool["retiring"] = json!(1);
        pool["registered_workers"] = json!(2);
        assert_eq!(
            render("worker", CONTROL_ID, "/project", Some(&pool)).unwrap_err(),
            RESIZING
        );
        let prompt = render("", CONTROL_ID, "/project", Some(&pool)).unwrap();
        assert!(prompt.contains("Stations: 3 (target 2)"));
        assert!(prompt.contains("working: 1; standby: 0; offline: 1; retiring: 1"));
        pool["desired_total"] = json!(3);
        assert_eq!(
            render("worker", CONTROL_ID, "/project", Some(&pool)).unwrap_err(),
            RESIZING
        );
    }

    #[test]
    fn busy_shrink_and_pending_expansion_block_recruitment_without_retiring_idle_slots() {
        let mut pool = pool();
        pool["working"] = json!(1);
        pool["offline"] = json!(1);
        pool["registered_workers"] = json!(2);
        for desired_total in [2, 4] {
            pool["desired_total"] = json!(desired_total);
            assert_eq!(
                render("worker", CONTROL_ID, "/project", Some(&pool)).unwrap_err(),
                RESIZING
            );
        }
        let prompt = render("", CONTROL_ID, "/project", Some(&pool)).unwrap();
        assert!(prompt.contains("working: 1; standby: 1; offline: 1; retiring: 0"));
    }

    #[test]
    fn offline_fallback_does_not_count_retiring_stations_as_vacancies() {
        let mut pool = pool();
        pool["total"] = json!(4);
        pool["desired_total"] = json!(3);
        pool["working"] = json!(1);
        pool["retiring"] = json!(1);
        pool["registered_workers"] = json!(3);
        pool.as_object_mut().unwrap().remove("offline");
        let prompt = render("", CONTROL_ID, "/project", Some(&pool)).unwrap();
        assert!(prompt.contains("working: 1; standby: 1; offline: 1; retiring: 1"));
    }

    #[test]
    fn malformed_resize_counts_and_inconsistent_four_part_totals_are_rejected() {
        for (field, value) in [
            ("desired_total", json!(null)),
            ("desired_total", json!(-1)),
            ("retiring", json!("0")),
            ("retiring", json!(1)),
            ("offline", json!(null)),
            ("offline", json!(3)),
            ("connected", json!("true")),
            ("closed", json!(0)),
            ("stopping", json!(null)),
        ] {
            let mut pool = pool();
            pool[field] = value;
            assert!(
                render("", CONTROL_ID, "/project", Some(&pool)).is_err(),
                "accepted {field}"
            );
        }
    }

    #[test]
    fn stopped_or_disconnected_pools_cannot_be_invited() {
        for (field, value) in [("stopping", true), ("closed", true), ("connected", false)] {
            let mut pool = pool();
            pool[field] = json!(value);
            for role in ["", "worker"] {
                assert_eq!(
                    render(role, CONTROL_ID, "/project", Some(&pool)).unwrap_err(),
                    "The worker pool is stopping or disconnected."
                );
            }
        }
    }
}
