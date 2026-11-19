use super::Error;

/// Pancake timestamps are ISO 8601 UTC strings. Round fractional expiry upward.
pub(crate) fn utc_timestamp(raw: &str) -> Option<u64> {
    if !raw.is_ascii() || raw.len() > 40 {
        return None;
    }
    let head = raw.get(..19)?;
    if &head[4..5] != "-"
        || &head[7..8] != "-"
        || &head[10..11] != "T"
        || &head[13..14] != ":"
        || &head[16..17] != ":"
    {
        return None;
    }
    let values = [
        &head[..4],
        &head[5..7],
        &head[8..10],
        &head[11..13],
        &head[14..16],
        &head[17..19],
    ];
    if values
        .iter()
        .any(|s| !s.bytes().all(|b| b.is_ascii_digit()))
    {
        return None;
    }
    let parsed: Vec<i32> = values
        .iter()
        .map(|s| s.parse())
        .collect::<Result<_, _>>()
        .ok()?;
    let tail = raw.get(19..)?;
    let tail = tail
        .strip_suffix('Z')
        .or_else(|| tail.strip_suffix("+00:00"))?;
    let fraction = if tail.is_empty() {
        ""
    } else {
        tail.strip_prefix('.')?
    };
    if (!tail.is_empty() && fraction.is_empty())
        || fraction.len() > 9
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let [year, month, day, hour, minute, second]: [i32; 6] = parsed.try_into().ok()?;
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=59).contains(&second)
    {
        return None;
    }
    let mut date: libc::tm = unsafe { std::mem::zeroed() };
    date.tm_year = year - 1900;
    date.tm_mon = month - 1;
    date.tm_mday = day;
    date.tm_hour = hour;
    date.tm_min = minute;
    date.tm_sec = second;
    // SAFETY: timegm receives a valid writable tm; normalize and reject impossible dates below.
    let timestamp = unsafe { libc::timegm(&mut date) };
    if [
        date.tm_year + 1900,
        date.tm_mon + 1,
        date.tm_mday,
        date.tm_hour,
        date.tm_min,
        date.tm_sec,
    ] != [year, month, day, hour, minute, second]
    {
        return None;
    }
    u64::try_from(timestamp)
        .ok()?
        .checked_add(u64::from(fraction.bytes().any(|b| b != b'0')))
}
pub(crate) fn expiry_label(timestamp: u64, offset: i32) -> Result<String, Error> {
    if !(-720..=840).contains(&offset) {
        return Err(Error::Configuration);
    }
    let timestamp = i64::try_from(timestamp)
        .map_err(|_| Error::Invalid)?
        .checked_add(i64::from(offset) * 60)
        .ok_or(Error::Invalid)?;
    let mut date: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers reference live correctly sized values; gmtime_r is thread-safe.
    if unsafe { libc::gmtime_r(&timestamp, &mut date) }.is_null() {
        return Err(Error::Invalid);
    }
    Ok(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC{}{:02}:{:02}",
        date.tm_year + 1900,
        date.tm_mon + 1,
        date.tm_mday,
        date.tm_hour,
        date.tm_min,
        date.tm_sec,
        if offset >= 0 { "+" } else { "-" },
        offset.abs() / 60,
        offset.abs() % 60
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn utc_expiry_is_strict_and_never_rounded_early() {
        assert_eq!(utc_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(utc_timestamp("1970-01-01T00:00:00.001Z"), Some(1));
        assert_eq!(utc_timestamp("1970-01-01T00:00:01+00:00"), Some(1));
        assert!(utc_timestamp("2024-02-29T00:00:00Z").is_some());
        for invalid in [
            "2023-02-29T00:00:00Z",
            "2024-02-30T00:00:00Z",
            "2024-02-01T00:00:60Z",
            "1970-01-01T00:00:01+08:00",
            "1970-01-01T00:00:01.Z",
        ] {
            assert_eq!(utc_timestamp(invalid), None);
        }
        assert_eq!(
            expiry_label(2_649_600, 480).unwrap(),
            "1970-02-01 00:00:00 UTC+08:00"
        );
    }
}
