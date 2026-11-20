//! Explicit civil instants at millisecond precision, independent of local TZ.
//! Commerce's expiry parser rounds up to seconds; interval arithmetic must not.
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) const MIN_UNIX_MS: i64 = -62_167_219_200_000;
pub(super) const MAX_UNIX_MS: i64 = 253_402_300_799_999;
const DAY_MS: i64 = 86_400_000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Point {
    unix_ms: Option<i64>,
    rfc3339: Option<String>,
}

impl Point {
    fn resolve(self) -> Result<i64, String> {
        match (self.unix_ms, self.rfc3339) {
            (Some(value), None) => bounded(value),
            (None, Some(value)) => parse(&value),
            _ => Err("each time point needs exactly one of unix_ms or rfc3339".into()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Interval {
    start: Point,
    end: Option<Point>,
}

pub(super) fn interval(arguments: Value, now_ms: i64) -> Result<Value, String> {
    let request: Interval = serde_json::from_value(arguments)
        .map_err(|_| "time.now op=interval expects start and optional end; each is {unix_ms: integer} or {rfc3339: string}")?;
    let start = request.start.resolve()?;
    let end_was_now = request.end.is_none();
    let end = request
        .end
        .map_or_else(|| bounded(now_ms), Point::resolve)?;
    // The accepted Gregorian range is far smaller than i64 milliseconds.
    let delta = end - start;
    let absolute = delta.unsigned_abs();
    Ok(json!({
        "schema_version":1,
        "start":{"unix_ms":start,"utc_rfc3339":format(start)?},
        "end":{"unix_ms":end,"utc_rfc3339":format(end)?},
        "end_was_now":end_was_now,
        "interval_ms":delta,"absolute_ms":absolute,
        "direction":if delta < 0 {"before"} else if delta > 0 {"after"} else {"same"},
        "duration":{"days":absolute / 86_400_000,"hours":absolute / 3_600_000 % 24,
            "minutes":absolute / 60_000 % 60,"seconds":absolute / 1000 % 60,"milliseconds":absolute % 1000},
        "precision":"milliseconds","clock":"UTC Unix epoch",
        "day_ms":86_400_000,"completion_permission":false
    }))
}

pub(super) fn now_ms() -> Result<i64, String> {
    let value = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(duration) => i64::try_from(duration.as_millis())
            .map_err(|_| "system clock exceeds supported Unix milliseconds")?,
        Err(error) => -i64::try_from(error.duration().as_nanos().div_ceil(1_000_000))
            .map_err(|_| "system clock precedes supported Unix milliseconds")?,
    };
    bounded(value)
}

fn bounded(value: i64) -> Result<i64, String> {
    if (MIN_UNIX_MS..=MAX_UNIX_MS).contains(&value) {
        Ok(value)
    } else {
        Err("time point must normalize to a UTC year in 0000..=9999".into())
    }
}

fn leap(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn year_days(year: i64) -> i64 {
    let previous = year - 1;
    365 * (year - 1970) + previous.div_euclid(4) - 1969 / 4 - previous.div_euclid(100)
        + 1969 / 100
        + previous.div_euclid(400)
        - 1969 / 400
}

fn month_days(year: i64, month: usize) -> i64 {
    match month {
        2 if leap(year) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn number(bytes: &[u8]) -> Result<i64, String> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return Err("RFC3339 date and time fields must contain ASCII digits".into());
    }
    Ok(bytes
        .iter()
        .fold(0, |value, byte| value * 10 + i64::from(byte - b'0')))
}

fn parse(raw: &str) -> Result<i64, String> {
    let bytes = raw.as_bytes();
    if !(20..=40).contains(&bytes.len())
        || !raw.is_ascii()
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !matches!(bytes[10], b'T' | b't')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return Err(
            "rfc3339 needs YYYY-MM-DDTHH:MM:SS[.sss] with Z or an explicit ±HH:MM offset".into(),
        );
    }
    let year = number(&bytes[..4])?;
    let month = number(&bytes[5..7])? as usize;
    let day = number(&bytes[8..10])?;
    let hour = number(&bytes[11..13])?;
    let minute = number(&bytes[14..16])?;
    let second = number(&bytes[17..19])?;
    if !(1..=12).contains(&month)
        || !(1..=month_days(year, month)).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return Err(
            "invalid Gregorian date/time; Unix millisecond intervals do not represent leap seconds"
                .into(),
        );
    }
    let mut cursor = 19;
    let mut fraction = 0;
    if bytes[cursor] == b'.' {
        cursor += 1;
        let begin = cursor;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            cursor += 1;
        }
        let digits = &bytes[begin..cursor];
        if digits.is_empty() || digits.len() > 9 || digits.iter().skip(3).any(|byte| *byte != b'0')
        {
            return Err("rfc3339 fractions need exact millisecond precision; nonzero sub-millisecond digits are not rounded".into());
        }
        let count = digits.len().min(3);
        fraction = number(&digits[..count])? * 10_i64.pow((3 - count) as u32);
    }
    let offset = match &bytes[cursor..] {
        [b'Z' | b'z'] => 0,
        [sign @ (b'+' | b'-'), a, b, b':', c, d] => {
            let hours = number(&[*a, *b])?;
            let minutes = number(&[*c, *d])?;
            if hours > 23 || minutes > 59 {
                return Err("RFC3339 offset must be within ±23:59".into());
            }
            // RFC3339 -00:00 still identifies a known UTC instant; only the
            // preferred local offset is unknown. Interval math uses UTC.
            (hours * 60 + minutes) * if *sign == b'-' { -1 } else { 1 }
        }
        _ => return Err("rfc3339 requires Z or a numeric ±HH:MM offset".into()),
    };
    let days =
        year_days(year) + (1..month).map(|month| month_days(year, month)).sum::<i64>() + day - 1;
    bounded(days * DAY_MS + (hour * 3600 + minute * 60 + second - offset * 60) * 1000 + fraction)
}

pub(super) fn format(unix_ms: i64) -> Result<String, String> {
    bounded(unix_ms)?;
    let days = unix_ms.div_euclid(DAY_MS);
    let within = unix_ms.rem_euclid(DAY_MS);
    let (mut low, mut high) = (0, 10_000);
    while low + 1 < high {
        let middle = (low + high) / 2;
        if year_days(middle) <= days {
            low = middle;
        } else {
            high = middle;
        }
    }
    let year = low;
    let mut ordinal = days - year_days(year);
    let mut month = 1;
    while ordinal >= month_days(year, month) {
        ordinal -= month_days(year, month);
        month += 1;
    }
    Ok(format!(
        "{year:04}-{month:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        ordinal + 1,
        within / 3_600_000,
        within / 60_000 % 60,
        within / 1000 % 60,
        within % 1000
    ))
}

pub(super) fn point_schema() -> Value {
    json!({"type":"object","additionalProperties":false,"properties":{
        "unix_ms":{"type":"integer","minimum":MIN_UNIX_MS,"maximum":MAX_UNIX_MS},
        "rfc3339":{"type":"string","minLength":20,"maxLength":40,
            "description":"RFC3339 with Z or numeric offset, exact millisecond precision; no leap seconds or nonzero sub-millisecond digits."}},
        "oneOf":[{"required":["unix_ms"],"not":{"required":["rfc3339"]}},
            {"required":["rfc3339"],"not":{"required":["unix_ms"]}}]})
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_interval_normalizes_offsets_midnight_and_negative_epoch_without_rounding() {
        for (input, expected) in [
            ("1970-01-01T00:00:00Z", 0),
            ("1970-01-01t00:00:00.001z", 1),
            ("1969-12-31T23:59:59.999Z", -1),
            ("1970-01-01T08:00:00.001+08:00", 1),
            ("1969-12-31T16:00:00-08:00", 0),
            ("1970-01-01T00:00:00-00:00", 0),
            ("2000-02-29T12:34:56.789000000Z", 951_827_696_789),
            ("0000-01-01T00:00:00Z", MIN_UNIX_MS),
            ("9999-12-31T23:59:59.999Z", MAX_UNIX_MS),
        ] {
            assert_eq!(parse(input).unwrap(), expected, "{input}");
            assert_eq!(parse(&format(expected).unwrap()).unwrap(), expected);
        }
        let value = interval(
            json!({"start":{"rfc3339":"2024-02-28T23:59:59.900Z"},
            "end":{"rfc3339":"2024-03-01T08:00:00.100+08:00"}}),
            0,
        )
        .unwrap();
        assert_eq!(value["interval_ms"], 86_400_200);
        assert_eq!(
            value["duration"],
            json!({"days":1,"hours":0,"minutes":0,"seconds":0,"milliseconds":200})
        );
        assert_eq!(
            interval(json!({"start":{"unix_ms":1001},"end":{"unix_ms":0}}), 0).unwrap()["interval_ms"],
            -1001
        );
        assert_eq!(
            interval(json!({"start":{"unix_ms":5}}), 12).unwrap()["interval_ms"],
            7
        );
    }

    #[test]
    fn time_interval_rejects_ambiguous_points_invalid_dates_and_lost_precision() {
        for invalid in [
            "2023-02-29T00:00:00Z",
            "1900-02-29T00:00:00Z",
            "2024-04-31T00:00:00Z",
            "2024-01-01T24:00:00Z",
            "2024-01-01T00:00:60Z",
            "2024-01-01T00:00:00",
            "2024-01-01T00:00:00+24:00",
            "2024-01-01T00:00:00+01:60",
            "2024-01-01T00:00:00.Z",
            "2024-01-01T00:00:00.0001Z",
            "0000-01-01T00:00:00+00:01",
            "9999-12-31T23:59:59-00:01",
        ] {
            assert!(parse(invalid).is_err(), "{invalid}");
        }
        for value in [
            json!({}),
            json!({"start":{}}),
            json!({"start":{"unix_ms":0,"rfc3339":"1970-01-01T00:00:00Z"}}),
            json!({"start":{"unix_ms":0,"timezone":"local"}}),
            json!({"start":{"unix_ms":MAX_UNIX_MS+1}}),
        ] {
            assert!(interval(value.clone(), 0).is_err(), "{value}");
        }
        // Exhaust the Gregorian leap cycle, including century exceptions.
        for year in 1800..=2200 {
            for month in 1..=12 {
                let raw = format!(
                    "{year:04}-{month:02}-{:02}T23:59:59.999Z",
                    month_days(year, month)
                );
                assert_eq!(format(parse(&raw).unwrap()).unwrap(), raw);
            }
        }
    }
}
