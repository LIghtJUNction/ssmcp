//! Whitelisted, read-only time-resource presentation for the native desktop.
use serde_json::Value;
use std::collections::BTreeSet;
use zgpui::bridge::protocol::BridgeError;

#[derive(Clone, Default)]
pub struct WaitSnapshot {
    pub lines: Vec<String>,
}

fn number(value: &Value, key: &str) -> Option<u64> {
    value.get(key).and_then(Value::as_u64)
}
fn duration(value: Option<u64>) -> String {
    match value {
        None => "未知".into(),
        Some(ms) if ms < 1000 => format!("{ms} 毫秒"),
        Some(ms) => format!(
            "{}:{:02}:{:02}",
            ms / 3_600_000,
            ms / 60_000 % 60,
            ms / 1000 % 60
        ),
    }
}

// Server fields are nonnegative Unix milliseconds. Preserve actual zero and
// use Gregorian calendar rules rather than the machine's local timezone.
fn timestamp(value: Option<u64>) -> String {
    let Some(ms) = value.filter(|ms| *ms <= 253_402_300_799_999) else {
        return "未知".into();
    };
    let leap = |year: u64| year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let mut days = ms / 86_400_000;
    let mut year = 1970;
    loop {
        let length = if leap(year) { 366 } else { 365 };
        if days < length {
            break;
        }
        days -= length;
        year += 1;
    }
    let lengths = [
        31,
        if leap(year) { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    let mut month = 0;
    while days >= lengths[month] {
        days -= lengths[month];
        month += 1;
    }
    format!(
        "{year:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} UTC",
        month + 1,
        days + 1,
        ms / 3_600_000 % 24,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        ms % 1000
    )
}

fn phase(entry: &Value) -> &'static str {
    if matches!(entry["status"].as_str(), Some("unknown" | "disconnected"))
        || entry["wait"]["outcome"] == "unknown"
    {
        return "unknown";
    }
    if let Some(durable) = entry.get("durable").filter(|value| value.is_object()) {
        return match durable["result_status"].as_str() {
            None if durable["state"] == "waiting" && entry["status"] == "running" => "active",
            Some("cancelled") => "cancelled",
            Some("timed_out") => "timed_out",
            Some("ready") if number(durable, "completed_at_ms").is_some() => match durable["state"]
                .as_str()
            {
                Some("acknowledged") => "acknowledged",
                Some("cached") if number(durable, "first_read_at_ms").is_some() => "read_attempt",
                Some("cached") => "unread",
                _ => "unknown",
            },
            _ => "unknown",
        };
    }
    match entry["status"].as_str() {
        Some("running") => "active",
        Some("returned") => "legacy",
        Some("cancelled") => "cancelled",
        Some("expired") => "window_expired",
        Some("elapsed") => "elapsed",
        _ => "unknown",
    }
}

fn label(phase: &str) -> &'static str {
    match phase {
        "active" => "进行中",
        "unread" => "已完成 · 等待读取",
        "read_attempt" => "已尝试读取 · 未确认",
        "acknowledged" => "已显式确认",
        "cancelled" => "观察已取消",
        "timed_out" => "等待生命周期已到期",
        "window_expired" => "本轮读取窗口已结束",
        "legacy" => "条件已触发 · 读取状态未知",
        "elapsed" => "计时已到期",
        _ => "结果未知",
    }
}

impl WaitSnapshot {
    pub fn parse(value: &Value) -> Result<Self, BridgeError> {
        let active = value["active"]
            .as_array()
            .ok_or_else(|| BridgeError::invalid("time snapshot active list missing"))?;
        let recent = value["recent"]
            .as_array()
            .ok_or_else(|| BridgeError::invalid("time snapshot retained list missing"))?;
        if active.len() > 256 || recent.len() > 256 {
            return Err(BridgeError::invalid("time snapshot exceeds display bounds"));
        }
        let mut seen = BTreeSet::new();
        let entries: Vec<_> = active
            .iter()
            .chain(recent)
            .filter(|entry| {
                entry["id"]
                    .as_str()
                    .is_some_and(|id| seen.insert((id, number(entry, "episode"))))
            })
            .collect();
        let mut samples = Vec::new();
        let mut completed = Vec::new();
        let mut cancelled = 0;
        let mut unknown = 0;
        let mut timed_out = 0;
        let mut window_expired = 0;
        for entry in &entries {
            let state = phase(entry);
            match state {
                "cancelled" => cancelled += 1,
                "unknown" => unknown += 1,
                "timed_out" => timed_out += 1,
                "window_expired" => window_expired += 1,
                _ => {}
            }
            if matches!(state, "unread" | "read_attempt" | "acknowledged")
                && let (Some(elapsed), Some(done)) = (
                    number(&entry["durable"], "elapsed_total_ms"),
                    number(&entry["durable"], "completed_at_ms"),
                )
            {
                samples.push(elapsed);
                completed.push(done);
            }
        }
        samples.sort_unstable();
        let count = samples.len();
        let mean = (count > 0).then(|| {
            ((samples.iter().map(|ms| u128::from(*ms)).sum::<u128>() + count as u128 / 2)
                / count as u128) as u64
        });
        let percentile = |p: usize| (count > 0).then(|| samples[(count * p).div_ceil(100) - 1]);
        let mut lines = vec![
            format!(
                "真实完成样本 {count} · 平均 {} · P50 {} · P95 {}",
                duration(mean),
                duration(percentile(50)),
                duration(percentile(95))
            ),
            format!(
                "取消 {cancelled} · 超时 {timed_out} · 未知 {unknown} · 读取窗口结束 {window_expired}"
            ),
            format!(
                "范围：当前保留的 {} 项，非全历史；仅统计有真实完成元数据的就绪观察。",
                entries.len()
            ),
            "分位数使用最近秩法；条件就绪不代表底层任务成功。".into(),
        ];
        if count > 0 {
            lines.push(format!(
                "样本完成时间 {} 至 {}",
                timestamp(completed.iter().copied().min()),
                timestamp(completed.iter().copied().max())
            ));
        }
        for (title, running) in [("进行中", true), ("保留的结果", false)] {
            lines.push(String::new());
            lines.push(title.into());
            for entry in entries
                .iter()
                .filter(|entry| (phase(entry) == "active") == running)
            {
                lines.push(format!(
                    "#{} · {} · {}",
                    super::text(entry, "id", 256),
                    super::text(entry, "label", 256),
                    label(phase(entry))
                ));
                lines.push(format!(
                    "实际已用 {}",
                    duration(number(entry, "elapsed_ms"))
                ));
                for (key, caption) in [("targets", "目标"), ("references", "引用")] {
                    for target in entry["wait"][key].as_array().into_iter().flatten().take(8) {
                        if let Some(text) = target.as_str() {
                            lines.push(format!("{caption} · {}", super::bounded(text, 1024)));
                        }
                    }
                }
                if let Some(durable) = entry.get("durable").filter(|value| value.is_object()) {
                    lines.push(format!(
                        "生命周期上限 {} · 本轮读取窗口 {}",
                        duration(number(durable, "lifetime_ms")),
                        duration(number(durable, "read_window_ms"))
                    ));
                    if phase(entry) == "active" {
                        lines.push(format!(
                            "生命周期剩余 {}",
                            duration(number(entry, "remaining_ms"))
                        ));
                    }
                    for (key, caption) in [
                        ("started_at_ms", "开始"),
                        ("completed_at_ms", "真实完成"),
                        ("first_read_at_ms", "首次读取尝试"),
                        ("last_read_at_ms", "最近读取尝试"),
                        ("acknowledged_at_ms", "显式确认"),
                        ("retained_until_ms", "服务端保留截止"),
                    ] {
                        lines.push(format!("{caption} · {}", timestamp(number(durable, key))));
                    }
                    lines.push(format!(
                        "首次读取尝试延迟 {}",
                        duration(number(durable, "delivery_lag_ms"))
                    ));
                } else {
                    lines.push(format!(
                        "本轮读取窗口 {} · 剩余 {}",
                        duration(number(&entry["wait"], "window_ms")),
                        duration(number(entry, "remaining_ms"))
                    ));
                    lines.push("完成 / 读取 / 确认时间 · 当前记录未提供".into());
                }
                if number(entry, "deadline_unix_ms").is_some() {
                    lines.push(format!(
                        "操作截止时间 · {}",
                        timestamp(number(entry, "deadline_unix_ms"))
                    ));
                }
                lines.push(String::new());
            }
        }
        lines.push("定时提示计划".into());
        if let Some(prompts) = value["prompts"].as_array() {
            if prompts.is_empty() {
                lines.push("当前连接没有定时提示计划".into());
            }
            for prompt in prompts.iter().take(256) {
                let status = match prompt["status"].as_str() {
                    Some("scheduled") => "已安排",
                    Some("paused") => "已暂停",
                    Some("completed") => "已完成",
                    Some("cancelled") => "已取消",
                    Some("expired") => "已到期",
                    Some("disconnected") => "连接已断开",
                    _ => "未知",
                };
                lines.push(format!("#{} · {status}", super::text(prompt, "id", 256)));
                lines.push(format!(
                    "下次触发 {} · 间隔 {}",
                    timestamp(number(prompt, "next_fire_unix_ms")),
                    if prompt.get("interval_ms").is_some_and(Value::is_null) {
                        "单次计划".into()
                    } else {
                        duration(number(prompt, "interval_ms"))
                    }
                ));
                let count = |key| {
                    number(prompt, key)
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "未知".into())
                };
                lines.push(format!(
                    "待确认 {} · 合并或错过 {} · 作用范围：当前原生连接",
                    count("pending_count"),
                    count("missed_count")
                ));
                lines.push(format!(
                    "计划到期 {}",
                    timestamp(number(prompt, "expires_at_unix_ms"))
                ));
                lines.push(String::new());
            }
        } else {
            lines.push("当前连接未提供计划摘要".into());
        }
        lines.push("读取时间表示服务端获取缓存内容的尝试；只有显式 ACK 表示调用方确认。".into());
        lines.push("读取窗口结束不会停止底层任务；计划正文和变量不会显示在此页。".into());
        Ok(Self { lines })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn unknown_and_cancelled_do_not_enter_actual_duration_samples_or_expose_private_text() {
        let value = json!({"active":[],"recent":[
            {"id":"ready","status":"returned","elapsed_ms":1000,"durable":{"state":"cached","result_status":"ready","completed_at_ms":1000,"elapsed_total_ms":1000,"private_result":"PRIVATE_BODY"}},
            {"id":"cancelled","status":"cancelled","durable":{"state":"cached","result_status":"cancelled","completed_at_ms":999999,"elapsed_total_ms":999999}},
            {"id":"unknown","status":"unknown","durable":{"state":"waiting","elapsed_total_ms":999999}}
        ],"prompts":[{"id":"prompt","status":"paused","pending_count":1,"missed_count":3,"template":"PRIVATE_PROMPT","variables":{"secret":"PRIVATE_VALUE"}}]});
        let text = WaitSnapshot::parse(&value).unwrap().lines.join("\n");
        assert!(text.contains("真实完成样本 1 · 平均 0:00:01"));
        assert!(text.contains("取消 1 · 超时 0 · 未知 1"));
        assert!(text.contains("待确认 1 · 合并或错过 3"));
        assert!(!text.contains("PRIVATE_"));
        assert_eq!(timestamp(None), "未知");
        assert_eq!(timestamp(Some(0)), "1970-01-01 00:00:00.000 UTC");
        assert_eq!(
            timestamp(Some(951_782_400_000)),
            "2000-02-29 00:00:00.000 UTC"
        );
    }
}
