//! Read-only quota presentation. The server supplies usage, capacity and reset time.
use crate::client::language::{Language, localized};
use serde_json::Value;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::client) enum Traffic {
    Unknown,
    Known {
        used_bytes: Option<u64>,
        limit_bytes: Option<u64>,
        reset: Option<(u64, i32)>,
    },
}

impl Traffic {
    pub(in crate::client) fn from_reply(reply: &Value) -> Self {
        let Some(traffic) = reply.get("traffic").and_then(Value::as_object) else {
            return Self::Unknown;
        };
        let used = traffic.get("used_bytes").and_then(Value::as_u64);
        let limit = match (traffic.get("limit_bytes"), traffic.get("remaining_bytes")) {
            (Some(Value::Null), Some(Value::Null)) => None,
            (Some(limit), Some(remaining)) => match (limit.as_u64(), remaining.as_u64(), used) {
                (Some(limit), Some(remaining), Some(used))
                    if remaining == limit.saturating_sub(used) =>
                {
                    Some(limit)
                }
                _ => return Self::Unknown,
            },
            _ => return Self::Unknown,
        };
        let offset = match traffic.get("utc_offset_minutes") {
            None => Some(0),
            Some(value) => value
                .as_i64()
                .and_then(|offset| (-720..=840).contains(&offset).then_some(offset as i32)),
        };
        let reset = traffic
            .get("reset_at")
            .and_then(Value::as_u64)
            .zip(offset)
            .filter(|(at, offset)| calendar(*at, *offset).is_some());
        Self::Known {
            used_bytes: used,
            limit_bytes: limit,
            reset,
        }
    }

    fn maximum(&self) -> String {
        match self {
            Self::Unknown => "?".into(),
            Self::Known {
                limit_bytes: None, ..
            } => "∞".into(),
            Self::Known {
                limit_bytes: Some(limit),
                ..
            } => bytes(*limit),
        }
    }

    #[cfg(test)]
    pub(super) fn progress(&self, width: u16) -> String {
        self.progress_in(width, Language::English)
    }

    pub(super) fn progress_in(&self, width: u16, language: Language) -> String {
        let width = usize::from(width);
        let Self::Known {
            used_bytes,
            limit_bytes,
            ..
        } = self
        else {
            return language.text("Quota unavailable", "额度不可用").into();
        };
        let maximum = localized!(language, "Max {}", "上限 {}", self.maximum());
        let Some(used) = used_bytes else {
            return localized!(language, "{maximum} · Used ?", "{maximum} · 已用 ?");
        };
        let Some(limit) = limit_bytes.filter(|limit| *limit > 0) else {
            let detail = localized!(
                language,
                "{maximum} · Used {}",
                "{maximum} · 已用 {}",
                bytes(*used)
            );
            return if detail.width() <= width {
                detail
            } else {
                maximum
            };
        };
        let percent = if *used > limit {
            language.text("Over", "超额").into()
        } else {
            format!("{}%", u128::from(*used) * 100 / u128::from(limit))
        };
        let detail = format!("{} / {maximum}", bytes(*used));
        if width >= detail.width() + 8 {
            let cells = width.saturating_sub(detail.width() + 3).clamp(3, 12);
            return format!("{} {detail}", bar(*used, limit, cells));
        }
        let cells = width
            .saturating_sub(percent.width() + maximum.width() + 4)
            .min(12);
        if cells >= 2 {
            format!("{percent} {} {maximum}", bar(*used, limit, cells))
        } else {
            format!("{percent} · {maximum}")
        }
    }

    #[cfg(test)]
    pub(super) fn reset_label(&self, width: u16) -> String {
        self.reset_label_in(width, Language::English)
    }

    pub(super) fn reset_label_in(&self, width: u16, language: Language) -> String {
        let Self::Known {
            reset: Some((at, offset)),
            ..
        } = self
        else {
            return language.text("Reset unknown", "重置日期未知").into();
        };
        let Some((year, month, day, hour, minute)) = calendar(*at, *offset) else {
            return language.text("Reset unknown", "重置日期未知").into();
        };
        let zone = if *offset == 0 {
            "UTC".into()
        } else if offset.abs() % 60 == 0 {
            format!("UTC{:+03}", offset / 60)
        } else {
            format!(
                "UTC{}{:02}:{:02}",
                if *offset < 0 { "-" } else { "+" },
                offset.abs() / 60,
                offset.abs() % 60
            )
        };
        let time = if hour == 0 && minute == 0 {
            String::new()
        } else {
            format!(" {hour:02}:{minute:02}")
        };
        let full = localized!(
            language,
            "Reset {year:04}-{month:02}-{day:02}{time} {zone}",
            "重置 {year:04}-{month:02}-{day:02}{time} {zone}"
        );
        if full.width() <= usize::from(width) {
            full
        } else if width >= 16 {
            localized!(
                language,
                "Reset {year:04}-{month:02}-{day:02}",
                "重置 {year:04}-{month:02}-{day:02}"
            )
        } else {
            localized!(
                language,
                "Reset {month:02}-{day:02}",
                "重置 {month:02}-{day:02}"
            )
        }
    }

    pub(super) fn compact(&self, language: Language) -> String {
        let reset = match self {
            Self::Known {
                reset: Some((at, offset)),
                ..
            } => calendar(*at, *offset).map(|(_, month, day, _, _)| format!("{month:02}-{day:02}")),
            _ => None,
        };
        localized!(
            language,
            "Max {} · Reset {}",
            "上限 {} · 重置 {}",
            self.maximum(),
            reset.as_deref().unwrap_or("?")
        )
    }
}

fn bar(used: u64, limit: u64, cells: usize) -> String {
    let filled = (u128::from(used.min(limit)) * cells as u128 / u128::from(limit)) as usize;
    format!("[{}{}]", "█".repeat(filled), "░".repeat(cells - filled))
}

/// Decimal units keep both very small and very large byte counts visible.
fn bytes(value: u64) -> String {
    const UNITS: [&str; 7] = ["B", "KB", "MB", "GB", "TB", "PB", "EB"];
    let mut scale = 1u64;
    let mut unit = 0usize;
    while unit + 1 < UNITS.len() && value / scale >= 1000 {
        scale *= 1000;
        unit += 1;
    }
    let whole = value / scale;
    let fraction = u128::from(value % scale) * 100 / u128::from(scale);
    let number = if fraction == 0 {
        whole.to_string()
    } else {
        format!("{whole}.{fraction:02}")
            .trim_end_matches('0')
            .to_owned()
    };
    format!("{number} {}", UNITS[unit])
}

/// Convert an explicit server timestamp at its fixed offset; never consult local time.
fn calendar(at: u64, offset: i32) -> Option<(i64, i64, i64, i64, i64)> {
    let seconds = i64::try_from(at)
        .ok()?
        .checked_add(i64::from(offset) * 60)?;
    let days = seconds.div_euclid(86_400).checked_add(719_468)?;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36524 - day_of_era / 146096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_from_march = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_from_march + 2) / 5 + 1;
    let month = month_from_march + if month_from_march < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    if !(1..=9999).contains(&year) {
        return None;
    }
    let daytime = seconds.rem_euclid(86_400);
    Some((year, month, day, daytime / 3600, daytime % 3600 / 60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn finite(used: u64, limit: u64) -> Traffic {
        Traffic::from_reply(&json!({"traffic":{
            "used_bytes":used,"limit_bytes":limit,
            "remaining_bytes":limit.saturating_sub(used)
        }}))
    }

    #[test]
    fn quota_uses_authoritative_usage_limit_and_does_not_sum_directional_counters() {
        let quota = Traffic::from_reply(&json!({"traffic":{
            "used_bytes":250_000_000u64,"limit_bytes":1_000_000_000u64,
            "remaining_bytes":750_000_000u64,"uploaded_bytes":200_000_000u64,
            "downloaded_bytes":240_000_000u64,"legacy_used_bytes":10_000_000u64,
            "free_bytes":700_000_000u64,"paid_bytes":300_000_000u64
        }}));
        assert_eq!(quota, finite(250_000_000, 1_000_000_000));
        assert!(quota.progress(24).starts_with("25%"));
        assert!(quota.progress(48).contains("250 MB / Max 1 GB"));
        let overridden = Traffic::from_reply(&json!({"traffic":{
            "used_bytes":250_000_000u64,"limit_bytes":1_000_000_000u64,
            "remaining_bytes":750_000_000u64,"free_bytes":42_000_000_000u64,"paid_bytes":0
        }}));
        assert_eq!(
            quota, overridden,
            "server overrides are not reconstructed from grants"
        );
    }

    #[test]
    fn quota_missing_usage_is_unknown_and_zero_or_over_limit_stays_finite() {
        for value in [
            json!({"traffic":{"limit_bytes":100,"remaining_bytes":0}}),
            json!({"traffic":{"limit_bytes":100,"remaining_bytes":50,"used_bytes":25}}),
            json!({"traffic":{"limit_bytes":100,"remaining_bytes":50,"used_bytes":"50"}}),
        ] {
            assert_eq!(Traffic::from_reply(&value), Traffic::Unknown);
        }
        for (used, limit) in [
            (0, 0),
            (0, 100),
            (100, 100),
            (101, 100),
            (u64::MAX, 1),
            (u64::MAX, u64::MAX),
        ] {
            let quota = finite(used, limit);
            for width in [20, 24, 48, 80] {
                let row = quota.progress(width);
                assert!(!row.contains('∞'));
                assert!(row.contains("Max"));
                assert!(row.width() <= usize::from(width), "{width}: {row}");
            }
        }
        assert!(finite(101, 100).progress(20).starts_with("Over"));
        assert_eq!(bar(0, 100, 4), "[░░░░]");
        assert_eq!(bar(50, 100, 4), "[██░░]");
        assert_eq!(bar(u64::MAX, 100, 4), "[████]");
    }

    #[test]
    fn quota_unlimited_requires_both_explicit_nulls_and_retains_unknown_usage() {
        let quota =
            Traffic::from_reply(&json!({"traffic":{"limit_bytes":null,"remaining_bytes":null}}));
        assert_eq!(quota.progress(24), "Max ∞ · Used ?");
        assert_eq!(quota.compact(Language::English), "Max ∞ · Reset ?");
        assert_eq!(
            Traffic::from_reply(&json!({"traffic":{"limit_bytes":null}})),
            Traffic::Unknown
        );
        let quota = Traffic::from_reply(&json!({"traffic":{
            "limit_bytes":null,"remaining_bytes":null,"used_bytes":u64::MAX
        }}));
        assert_eq!(quota.progress(20), "Max ∞");
        assert!(quota.progress(48).contains("18.44 EB"));
    }

    #[test]
    fn quota_reset_uses_explicit_server_timestamp_and_offset_at_year_and_leap_boundaries() {
        for (at, offset, expected) in [
            (1_796_054_400, 480, "Reset 2026-12-01 UTC+08"),
            (1_798_732_800, 480, "Reset 2027-01-01 UTC+08"),
            (1_835_452_800, 480, "Reset 2028-03-01 UTC+08"),
            (43_200, -720, "Reset 1970-01-01 UTC-12"),
        ] {
            let quota = Traffic::from_reply(&json!({"traffic":{
                "used_bytes":0,"limit_bytes":100,"remaining_bytes":100,
                "reset_at":at,"utc_offset_minutes":offset
            }}));
            assert_eq!(quota.reset_label(48), expected);
            assert!(quota.reset_label(20).width() <= 20);
            assert!(quota.reset_label(12).width() <= 12);
        }
        assert_eq!(calendar(1_796_054_400, 330), Some((2026, 11, 30, 21, 30)));
        assert_eq!(calendar(u64::MAX, 0), None);
        assert_eq!(finite(1, 100).reset_label(40), "Reset unknown");
        for reset in [Value::Null, json!(-1), json!("tomorrow"), json!(u64::MAX)] {
            let quota = Traffic::from_reply(&json!({"traffic":{
                "used_bytes":0,"limit_bytes":100,"remaining_bytes":100,"reset_at":reset
            }}));
            assert_eq!(quota.reset_label(40), "Reset unknown");
            assert!(quota.progress(40).contains("Max 100 B"));
        }
    }

    #[test]
    fn quota_decimal_units_do_not_hide_tiny_counts_or_overflow_large_counts() {
        for (value, expected) in [
            (0, "0 B"),
            (1, "1 B"),
            (999, "999 B"),
            (1000, "1 KB"),
            (9999, "9.99 KB"),
            (250_000_000, "250 MB"),
            (1_000_000_000, "1 GB"),
            (u64::MAX, "18.44 EB"),
        ] {
            assert_eq!(bytes(value), expected);
        }
    }
}
