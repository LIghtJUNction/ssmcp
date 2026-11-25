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

pub(super) fn groups(events: &VecDeque<Event>, history: &DisplayHistory) -> Vec<Group> {
    let eligible = |e: &Event| {
        if !e.owned_by(history) || e.chat_expanded || e.historical || e.truncated {
            return None;
        }
        e.meta.notice.as_ref()?.progress
    };
    let mut groups = Vec::new();
    let mut start = 0;
    while start < events.len() {
        let head = &events[start];
        let Some(key) = eligible(head) else {
            start += 1;
            continue;
        };
        let mut end = start + 1;
        while end < events.len() {
            let next = &events[end];
            let same_recipient = match (&head.meta.scope, &next.meta.scope) {
                (Some(a), Some(b)) => {
                    a.connection() == b.connection()
                        && a.thread() == b.thread()
                        && a.attribution() == b.attribution()
                }
                (None, None) => true,
                _ => false,
            };
            if !same_recipient || eligible(next) != Some(key) {
                break;
            }
            end += 1;
        }
        let count = end - start;
        if count > 1 {
            groups.push(Group {
                head: head.id,
                start,
                members: (start..end).collect(),
                title: format!("{} ×{count}", head.meta.notice.as_ref().unwrap().summary),
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
