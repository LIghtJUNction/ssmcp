//! Wall-clock labels for events in the worker's Logs and Progress views.

fn local_clock(at_ms: u64) -> Option<(i32, i32, i32)> {
    let seconds = libc::time_t::try_from(at_ms / 1000).ok()?;
    let mut date = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: the OS reads one time_t and writes to caller-owned, aligned tm storage.
    #[cfg(unix)]
    let ok = !unsafe { libc::localtime_r(&seconds, date.as_mut_ptr()) }.is_null();
    #[cfg(windows)]
    let ok = unsafe { libc::localtime_s(date.as_mut_ptr(), &seconds) } == 0;
    if !ok {
        return None;
    }
    // SAFETY: a successful conversion initialized the tm above.
    let date = unsafe { date.assume_init() };
    Some((date.tm_hour, date.tm_min, date.tm_sec))
}

pub(super) fn label(at_ms: u64, width: usize) -> String {
    if width < 28 {
        return String::new();
    }
    match (local_clock(at_ms), width >= 40) {
        (Some((h, m, s)), true) => format!("{h:02}:{m:02}:{s:02}  "),
        (Some((h, m, _)), false) => format!("{h:02}:{m:02}  "),
        (None, true) => "--:--:--  ".into(),
        (None, false) => "--:--  ".into(),
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn event_clock_uses_the_terminal_zone_at_the_event_instant() {
        // A child test process can select its own zone without changing other tests.
        const MARKER: &str = "SSMCP_TEST_WORKER_CLOCK_ZONE";
        if let Some(zone) = std::env::var_os(MARKER) {
            match zone.to_str().unwrap() {
                "NPT-5:45" => {
                    let stamp = 1_700_000_000_000;
                    assert_eq!(label(stamp, 27), "");
                    for width in [28, 39] {
                        assert_eq!(label(stamp, width), "03:58  ");
                    }
                    assert_eq!(label(stamp, 40), "03:58:20  ");
                }
                "EST5EDT,M3.2.0/2,M11.1.0/2" => {
                    assert_eq!(label(1_710_053_999_000, 40), "01:59:59  ");
                    assert_eq!(label(1_710_054_000_000, 40), "03:00:00  ");
                }
                _ => unreachable!(),
            }
            return;
        }
        for zone in ["NPT-5:45", "EST5EDT,M3.2.0/2,M11.1.0/2"] {
            let result = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    concat!(
                        module_path!(),
                        "::event_clock_uses_the_terminal_zone_at_the_event_instant"
                    )
                    .split_once("::")
                    .unwrap()
                    .1,
                ])
                .env(MARKER, zone)
                .env("TZ", zone)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&result.stdout);
            assert!(
                result.status.success() && stdout.contains("1 passed"),
                "{zone}: {stdout}"
            );
        }
    }
}
