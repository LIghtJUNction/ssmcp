//! A folded conversation row keeps each original journal event selectable in detail.
use super::{DisplayHistory, Event, read_activity::Group};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::VecDeque;

pub(super) fn identity(event: &Value) -> Option<[u8; 32]> {
    let data = &event["data"];
    if event["kind"] != "worker.progress" || data["task_status"] != "working" {
        return None;
    }
    let client = event["client_id"].as_str()?.trim();
    let worker = event["actor_id"]
        .as_str()
        .or_else(|| data["participant_id"].as_str())?
        .trim();
    let task = data["task_id"].as_str()?.trim();
    if client.is_empty() || worker.is_empty() || task.is_empty() {
        return None;
    }
    // A station can be reassigned within one pool, including while a retry of
    // its preceding task is still visible. The sequence itself is not identity.
    let key = (
        client,
        worker,
        task,
        &data["participant_id"],
        &data["slot"],
        &data["epoch"],
        &data["attempt"],
        &data["source_cursor"]["generation"],
    );
    Some(Sha256::digest(serde_json::to_vec(&key).ok()?).into())
}

pub(super) fn repository_summary(event: &Value) -> Option<String> {
    let data = &event["data"];
    let label = |key: &str| {
        data[key]
            .as_str()
            .filter(|name| crate::wire::repository_events::valid_label(name))
    };
    match event["kind"].as_str()? {
        crate::wire::repository_events::PULL_REQUEST_OPENED => Some(format!(
            "{} · 新 PR #{}",
            label("remote")?,
            data["number"].as_u64()?
        )),
        crate::wire::repository_events::UPSTREAM_CHANGED => {
            let (branch, upstream) = (label("branch")?, label("upstream")?);
            let (ahead, behind) = (data["ahead"].as_u64()?, data["behind"].as_u64()?);
            Some(match (ahead, behind) {
                (0, 0) => format!("{branch} 与 {upstream} 已同步"),
                (_, 0) => format!("{branch} 比 {upstream} 领先 {ahead} 个提交"),
                (0, _) => format!("{upstream} 比 {branch} 领先 {behind} 个提交"),
                _ => format!("{branch} 有 {ahead} 个独有提交，{upstream} 有 {behind} 个"),
            })
        }
        _ => None,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Repeat {
    Progress([u8; 32]),
    Gap([u8; 32]),
    Standby([u8; 32]),
    Policy([u8; 32]),
    PromptRead,
}

impl Repeat {
    pub(super) fn standby(text: &str) -> Option<Self> {
        // Additional instructions from a newer relay must get their own row.
        text.ends_with(" connected external worker(s) are standing by. Use ssmcp.worker.submit for independent work.")
            .then(|| Self::Standby(Sha256::digest(text).into()))
    }

    pub(super) fn policy(text: &str) -> Option<Self> {
        text.strip_prefix("Continue authorized work; otherwise await these events (")?
            .strip_suffix(" ms, same wait_id). ACK processed cursors. ui.wait delivers input.")?
            .parse::<u64>()
            .ok()?;
        Some(Self::Policy(Sha256::digest(text).into()))
    }

    pub(super) fn gap(value: &Value) -> Option<Self> {
        let sub = value["subscription_id"].as_str()?;
        if sub.is_empty()
            || sub.trim() != sub
            || value["events"]
                .as_array()
                .is_some_and(|events| !events.is_empty())
        {
            return None;
        }
        let lost = &value["retention_gap"];
        let after = match value["status"].as_str()? {
            "retention_gap" => lost["episode_after"]
                .as_u64()
                .or_else(|| lost["after"].as_u64())?,
            "unavailable" => {
                let error = value["error"]
                    .as_str()?
                    .strip_prefix("events_gap: cursor ")?;
                let (after, boundary) = error.split_once(" precedes retained cursor ")?;
                let retained = if let Some((retained, latest)) =
                    boundary.split_once("; latest cursor ")
                {
                    latest.strip_suffix("; resubscribe with an explicit retained cursor after reconciling the gap")?
                        .parse::<u64>().ok()?;
                    retained
                } else {
                    boundary
                };
                retained.parse::<u64>().ok()?;
                after.parse().ok()?
            }
            _ => return None,
        };
        let key = (
            sub,
            after,
            &value["status"],
            &value["client_id"],
            &value["actor_id"],
        );
        Some(Self::Gap(
            Sha256::digest(serde_json::to_vec(&key).ok()?).into(),
        ))
    }
}

fn repeat(event: &Event, history: &DisplayHistory) -> Option<Repeat> {
    if !event.owned_by(history) || event.chat_expanded || event.historical || event.truncated {
        return None;
    }
    if let Some(notice) = &event.meta.notice {
        return notice.repeat;
    }
    (event.meta.prompt_read
        && event.meta.status == Some(super::Status::Done)
        && event.text == "[RUN] ssmcp.time.prompt\n"
        && event.image.is_none())
    .then_some(Repeat::PromptRead)
}

fn same_recipient(first: &Event, next: &Event) -> bool {
    match (&first.meta.scope, &next.meta.scope) {
        (Some(a), Some(b)) => {
            a.connection() == b.connection()
                && a.thread() == b.thread()
                && a.attribution() == b.attribution()
        }
        (None, None) => true,
        _ => false,
    }
}

pub(super) fn groups(events: &VecDeque<Event>, history: &DisplayHistory) -> Vec<Group> {
    let mut groups = Vec::new();
    let mut start = 0;
    while start < events.len() {
        let Some(first) = repeat(&events[start], history) else {
            start += 1;
            continue;
        };
        // At most four headings can interleave in a run. A different value for
        // any heading starts a new run, including a changed cursor or pool.
        let mut by_kind = vec![(first, vec![start])];
        let mut end = start + 1;
        while end < events.len() {
            let next = &events[end];
            if next.meta.read_bookkeeping
                && next.meta.kind == super::Kind::Log
                && !matches!(next.tone, "error" | "warning")
                && !next.meta.status.is_some_and(super::Status::failed)
                && (next.meta.history == DisplayHistory::Global
                    || (next.owned_by(history) && same_recipient(&events[start], next)))
            {
                end += 1;
                continue;
            }
            let Some(key) = repeat(next, history) else {
                break;
            };
            if !same_recipient(&events[start], next) {
                break;
            }
            if let Some((previous, members)) = by_kind.iter_mut().find(|(previous, _)| {
                std::mem::discriminant(previous) == std::mem::discriminant(&key)
            }) {
                if *previous != key {
                    break;
                }
                members.push(end);
            } else if matches!(first, Repeat::Progress(_)) || matches!(key, Repeat::Progress(_)) {
                break;
            } else {
                by_kind.push((key, vec![end]));
            }
            end += 1;
        }
        for (_, members) in by_kind.into_iter().filter(|(_, members)| members.len() > 1) {
            let start = members[0];
            let head = &events[start];
            let label = head
                .meta
                .notice
                .as_ref()
                .map_or("ssmcp.time.prompt · done", |n| &n.summary);
            groups.push(Group {
                head: head.id,
                start,
                title: format!("{label} ×{}", members.len()),
                members,
                expanded: false,
            });
        }
        start = end;
    }
    groups
}

#[cfg(test)]
mod tests {
    use super::super::{EventScope, EventView, Ui};
    use super::*;
    use serde_json::json;

    fn add(view: &mut EventView, value: Value) {
        let text = format!("WORKER STATE EVENTS — retained until explicit events.ack\n{value}");
        view.ingest_event(
            "message",
            &json!({"type":"ssmcp.message","role":"tool","format":"plain",
            "kind":"input","input_origin":"worker_event","text":text}),
            b"",
        );
    }

    #[test]
    fn adjacent_worker_progress_folds_with_count_and_keeps_detail_search_and_navigation() {
        let plain = Ui {
            color: false,
            ..Ui::default()
        };
        let mut view = EventView::new();
        view.set_scope(EventScope::Conversation);
        let event = json!({"kind":"worker.progress","client_id":"pool","actor_id":"worker",
            "data":{"task_status":"working","task_id":"task","slot":1,"epoch":1}});
        let mut first = event.clone();
        first["id"] = json!("first-event");
        add(&mut view, first);
        let mut second = event.clone();
        second["id"] = json!("second-event");
        add(&mut view, second);
        let mut failed = event.clone();
        failed["data"]["task_status"] = json!("failed");
        add(&mut view, failed);
        for (field, value) in [("epoch", json!(2)), ("task_id", json!("another-task"))] {
            let mut another = event.clone();
            another["data"][field] = value;
            add(&mut view, another);
        }
        let rows = view.render(100, 20, &plain, true).join("\n");
        assert!(rows.contains("工位 2 · 进度更新 ×2"), "{rows}");
        assert!(rows.contains("任务失败"));
        assert_eq!(view.visible_ids(), vec![0, 2, 3, 4]);
        view.select(isize::MIN);
        assert!(view.open());
        assert_eq!(view.visible_ids(), vec![0, 1, 2, 3, 4]);
        let details = view.render(190, 40, &plain, true).join("\n");
        assert!(details.contains("first-event") && details.contains("second-event"));
        assert!(view.back());
        view.filter("second-event", 0);
        assert_eq!(view.visible_ids(), vec![1]);
        view.filter("", 0);
        view.set_scope(EventScope::All);
        assert_eq!(view.visible_ids(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn structured_gap_uses_subscription_and_original_loss_episode() {
        let a = json!({"status":"retention_gap","subscription_id":"one",
            "retention_gap":{"after":30,"episode_after":24,"through":31,"resume_after":33}});
        let key = Repeat::gap(&a);
        assert!(key.is_some());
        let mut next = a.clone();
        next["retention_gap"]["after"] = json!(31);
        next["retention_gap"]["through"] = json!(37);
        assert!(Repeat::gap(&next) == key);
        next["retention_gap"]["episode_after"] = json!(31);
        assert!(Repeat::gap(&next) != key);
        next = a.clone();
        next["subscription_id"] = json!("two");
        assert!(Repeat::gap(&next) != key);
        next["subscription_id"] = Value::Null;
        assert!(Repeat::gap(&next).is_none());
        let unexpected = json!({"status":"unavailable", "subscription_id":"one",
            "error":"events_gap: cursor 31 precedes retained cursor 32; a different diagnostic"});
        assert!(Repeat::gap(&unexpected).is_none());
    }

    #[test]
    fn repository_notices_use_remote_and_local_directions() {
        let mut event = json!({"kind":"repo.upstream_changed","data":{"branch":"main",
            "upstream":"origin/main","ahead":0,"behind":3}});
        assert_eq!(
            repository_summary(&event).unwrap(),
            "origin/main 比 main 领先 3 个提交"
        );
        event["data"]["ahead"] = json!(2);
        assert_eq!(
            repository_summary(&event).unwrap(),
            "main 有 2 个独有提交，origin/main 有 3 个"
        );
        assert_eq!(
            repository_summary(
                &json!({"kind":"repo.pull_request_opened","data":{"remote":"fork","number":18}})
            )
            .unwrap(),
            "fork · 新 PR #18"
        );
    }
}
