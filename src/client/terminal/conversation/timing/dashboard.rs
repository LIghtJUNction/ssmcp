//! Pure wait-panel projection. Reading or rendering cannot acknowledge a result.
use super::*;
use crate::wire::wait_display::{DurableState, DurableStatus};
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Active,
    Unread,
    ReadAttempt,
    Acknowledged,
    Cancelled,
    TimedOut,
    WindowExpired,
    Unknown,
    Legacy,
    Elapsed,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Active => "进行中",
            Self::Unread => "已完成 · 等待读取",
            Self::ReadAttempt => "已尝试读取 · 未确认",
            Self::Acknowledged => "已显式确认",
            Self::Cancelled => "观察已取消",
            Self::TimedOut => "等待生命周期已到期",
            Self::WindowExpired => "本轮读取窗口已结束",
            Self::Unknown => "结果未知",
            Self::Legacy => "条件已触发 · 读取状态未知",
            Self::Elapsed => "计时已到期",
        }
    }
}

pub(super) fn phase(entry: &Entry) -> Phase {
    if matches!(entry.status, Status::Unknown | Status::Disconnected)
        || entry
            .wait
            .as_ref()
            .is_some_and(|wait| wait.outcome == crate::wire::wait_display::Outcome::Unknown)
    {
        return Phase::Unknown;
    }
    if let Some(durable) = &entry.durable {
        return match durable.result_status {
            None if durable.state == DurableState::Waiting && entry.status == Status::Running => {
                Phase::Active
            }
            Some(DurableStatus::Cancelled) => Phase::Cancelled,
            Some(DurableStatus::TimedOut) => Phase::TimedOut,
            Some(DurableStatus::Ready) if durable.completed_at_ms.is_some() => {
                match durable.state {
                    DurableState::Acknowledged => Phase::Acknowledged,
                    DurableState::Cached if durable.first_read_at_ms.is_some() => {
                        Phase::ReadAttempt
                    }
                    DurableState::Cached => Phase::Unread,
                    _ => Phase::Unknown,
                }
            }
            _ => Phase::Unknown,
        };
    }
    match entry.status {
        Status::Running => Phase::Active,
        Status::Returned => Phase::Legacy,
        Status::Cancelled => Phase::Cancelled,
        Status::Expired => Phase::WindowExpired,
        Status::Elapsed => Phase::Elapsed,
        _ => Phase::Unknown,
    }
}

#[derive(Default, Debug)]
pub(super) struct Stats {
    pub(super) retained: usize,
    pub(super) count: usize,
    pub(super) mean: Option<u64>,
    pub(super) p50: Option<u64>,
    pub(super) p95: Option<u64>,
    pub(super) cancelled: usize,
    pub(super) unknown: usize,
    pub(super) timed_out: usize,
    pub(super) window_expired: usize,
    earliest: Option<u64>,
    latest: Option<u64>,
}

impl Stats {
    pub(super) fn from_snapshot(snapshot: &Snapshot) -> Self {
        let mut result = Self::default();
        let mut seen = BTreeSet::new();
        let mut samples = Vec::new();
        for entry in snapshot.active.iter().chain(&snapshot.recent) {
            if (entry.wait.is_none() && entry.durable.is_none())
                || !seen.insert((&entry.id, entry.episode))
            {
                continue;
            }
            result.retained += 1;
            let state = phase(entry);
            match state {
                Phase::Cancelled => result.cancelled += 1,
                Phase::Unknown => result.unknown += 1,
                Phase::TimedOut => result.timed_out += 1,
                Phase::WindowExpired => result.window_expired += 1,
                _ => {}
            }
            if matches!(
                state,
                Phase::Unread | Phase::ReadAttempt | Phase::Acknowledged
            ) && let Some(durable) = &entry.durable
                && durable.result_status == Some(DurableStatus::Ready)
                && let Some(completed) = durable.completed_at_ms
            {
                samples.push(durable.elapsed_total_ms);
                result.earliest = Some(
                    result
                        .earliest
                        .map_or(completed, |value| value.min(completed)),
                );
                result.latest = Some(
                    result
                        .latest
                        .map_or(completed, |value| value.max(completed)),
                );
            }
        }
        samples.sort_unstable();
        result.count = samples.len();
        if !samples.is_empty() {
            let count = samples.len() as u128;
            result.mean = Some(
                ((samples.iter().map(|ms| u128::from(*ms)).sum::<u128>() + count / 2) / count)
                    as u64,
            );
            result.p50 = Some(samples[(samples.len() * 50).div_ceil(100) - 1]);
            result.p95 = Some(samples[(samples.len() * 95).div_ceil(100) - 1]);
        }
        result
    }
}

fn optional_duration(value: Option<u64>) -> String {
    value.map(wait_duration).unwrap_or_else(|| "未知".into())
}

fn stamp(value: Option<u64>) -> String {
    value
        .and_then(|value| i64::try_from(value).ok())
        .and_then(|value| crate::runtime::time::utc_at(value).ok())
        .unwrap_or_else(|| "未知".into())
}

fn signed_stamp(value: Option<i64>) -> String {
    value
        .and_then(|value| crate::runtime::time::utc_at(value).ok())
        .unwrap_or_else(|| "未知".into())
}

pub(super) fn entry_lines(entry: &Entry) -> Vec<String> {
    let mut rows = vec![
        format!(
            "#{} · {}",
            panel::safe_text(&entry.id),
            panel::safe_text(&entry.label)
        ),
        format!(
            "{} · 实际已用 {}",
            phase(entry).label(),
            wait_duration(entry.elapsed_ms)
        ),
    ];
    if let Some(wait) = &entry.wait {
        rows.push(format!(
            "模式 {} · {}",
            wait_mode(wait.mode),
            panel::safe_text(&wait.operation)
        ));
        for target in &wait.targets {
            rows.push(format!("目标 · {}", panel::safe_text(target)));
        }
        for reference in &wait.references {
            rows.push(format!("引用 · {}", panel::safe_text(reference)));
        }
    }
    if let Some(durable) = &entry.durable {
        rows.push(format!(
            "生命周期上限 {} · 剩余 {}",
            wait_duration(durable.lifetime_ms),
            if entry.status == Status::Running {
                optional_duration(entry.remaining_ms)
            } else {
                "已结束或未知".into()
            }
        ));
        rows.push(format!(
            "本轮读取窗口 {}",
            wait_duration(durable.read_window_ms)
        ));
        for (label, value) in [
            ("开始", Some(durable.started_at_ms)),
            ("真实完成", durable.completed_at_ms),
            ("首次读取尝试", durable.first_read_at_ms),
            ("最近读取尝试", durable.last_read_at_ms),
            ("显式确认", durable.acknowledged_at_ms),
            ("服务端保留截止", durable.retained_until_ms),
        ] {
            rows.push(format!("{label} · {}", stamp(value)));
        }
        rows.push(format!(
            "首次读取尝试延迟 {} · 读取窗口 {} 次 · 重读 {} 次",
            optional_duration(durable.delivery_lag_ms),
            durable.read_windows,
            durable.replay_count
        ));
    } else {
        if let Some(wait) = &entry.wait {
            let mut window = format!(
                "本轮读取窗口 {}",
                wait.window_ms
                    .map(wait_budget)
                    .unwrap_or_else(|| "未知".into())
            );
            if wait.outcome == crate::wire::wait_display::Outcome::Pending
                && let Some(remaining) = entry.remaining_ms
            {
                window.push_str(&format!(
                    " · 剩余 ≤ {}",
                    wait_budget(live_wait_bound(remaining, wait.window_ms))
                ));
            }
            rows.push(window);
        }
        rows.push("完成 / 读取 / 确认时间 · 当前记录未提供".into());
    }
    if entry.deadline_unix_ms.is_some() {
        rows.push(format!(
            "操作截止时间 · {}",
            signed_stamp(entry.deadline_unix_ms)
        ));
    }
    if entry.status == Status::Running && entry.remaining_ms == Some(0) {
        rows.push("截止时间已到，仍等待实际操作返回。".into());
    }
    if entry.cancellable {
        rows.push(format!(
            "/wait cancel {} · 使用完整编号 exact ID",
            panel::safe_text(&entry.id)
        ));
    }
    if let Some(attribution) = &entry.attribution
        && let Some(agent) = &attribution.agent
    {
        rows.push(format!(
            "观察者 · {} @ {}",
            panel::safe_text(&agent.name),
            panel::safe_text(&attribution.account.login)
        ));
    }
    rows
}

pub(super) fn lines(snapshot: &Snapshot) -> Vec<(String, PanelTone)> {
    let stats = Stats::from_snapshot(snapshot);
    let mut rows = vec![
        ("等待与计时 · /wait".into(), PanelTone::Heading),
        (String::new(), PanelTone::Normal),
        (
            format!(
                "真实完成样本 {} · 平均 {}",
                stats.count,
                optional_duration(stats.mean)
            ),
            PanelTone::Normal,
        ),
        (
            format!(
                "P50 {} · P95 {}",
                optional_duration(stats.p50),
                optional_duration(stats.p95)
            ),
            PanelTone::Normal,
        ),
        (
            format!(
                "取消 {} · 超时 {} · 未知 {} · 读取窗口结束 {}",
                stats.cancelled, stats.timed_out, stats.unknown, stats.window_expired
            ),
            PanelTone::Muted,
        ),
        (
            format!("范围：当前保留的 {} 项观察，非全历史。", stats.retained),
            PanelTone::Muted,
        ),
        (
            "仅统计具有真实完成元数据的就绪记录；分位数使用最近秩法。".into(),
            PanelTone::Muted,
        ),
    ];
    if stats.count > 0 {
        rows.push((
            format!(
                "样本完成时间 {} 至 {}",
                stamp(stats.earliest),
                stamp(stats.latest)
            ),
            PanelTone::Muted,
        ));
    }
    for (title, entries) in [
        ("进行中", &snapshot.active),
        ("保留的结果", &snapshot.recent),
    ] {
        rows.push((String::new(), PanelTone::Normal));
        rows.push((title.into(), PanelTone::Heading));
        if entries.is_empty() {
            rows.push(("当前没有记录".into(), PanelTone::Muted));
        }
        for entry in entries {
            rows.extend(
                entry_lines(entry)
                    .into_iter()
                    .map(|line| (line, PanelTone::Normal)),
            );
            rows.push((String::new(), PanelTone::Normal));
        }
    }
    rows.push(("定时提示计划".into(), PanelTone::Heading));
    if snapshot.prompts.is_empty() {
        rows.push(("当前连接没有定时提示计划".into(), PanelTone::Muted));
    }
    for prompt in &snapshot.prompts {
        use crate::runtime::time::PromptStatus;
        let status = match prompt.status {
            PromptStatus::Scheduled => "已安排",
            PromptStatus::Paused => "已暂停",
            PromptStatus::Completed => "已完成",
            PromptStatus::Cancelled => "已取消",
            PromptStatus::Expired => "已到期",
            PromptStatus::Disconnected => "连接已断开",
        };
        rows.push((
            format!("#{} · {status}", panel::safe_text(&prompt.id)),
            PanelTone::Normal,
        ));
        rows.push((
            format!(
                "下次触发 {} · 间隔 {}",
                signed_stamp(prompt.next_fire_unix_ms),
                prompt
                    .interval_ms
                    .map(wait_duration)
                    .unwrap_or_else(|| "单次计划".into())
            ),
            PanelTone::Normal,
        ));
        rows.push((
            format!(
                "待确认 {} · 合并或错过 {} · 作用范围：当前原生连接",
                prompt.pending_count, prompt.missed_count
            ),
            PanelTone::Normal,
        ));
        rows.push((
            format!("计划到期 {}", signed_stamp(Some(prompt.expires_at_unix_ms))),
            PanelTone::Muted,
        ));
        rows.push((String::new(), PanelTone::Normal));
    }
    for line in [
        "读取时间表示服务端获取缓存内容的尝试；只有显式 ACK 表示调用方确认。",
        "本轮读取窗口是等待预算；窗口结束不会停止底层任务。",
        "计时到期不代表任务完成；取消计时不会停止任务。",
        "条件就绪不代表底层任务成功。计划正文和变量不会显示在面板。",
    ] {
        rows.push((line.into(), PanelTone::Muted));
    }
    rows
}

pub(super) fn card(entry: &Entry) -> Option<WaitCard> {
    let wait = entry.wait.as_ref()?;
    Some(WaitCard {
        key: format!(
            "wait-observation\0{}\0{}",
            entry.id,
            entry.episode.unwrap_or(0)
        ),
        heading: format!(
            "{} · {} · {} · {}",
            wait_mode(wait.mode),
            phase(entry).label(),
            wait_duration(entry.elapsed_ms),
            panel::fit(
                &panel::safe_text(
                    wait.targets
                        .first()
                        .map(String::as_str)
                        .unwrap_or("等待目标")
                ),
                22
            )
        ),
        detail: entry_lines(entry).join("\n"),
        entry: entry.clone(),
    })
}
