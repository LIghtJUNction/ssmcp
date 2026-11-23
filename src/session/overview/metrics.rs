//! Process measurements, never agent/model usage estimates.
use serde_json::{Value, json};
use std::{collections::HashMap, time::Instant};

#[derive(Clone, Copy)]
pub(super) struct Sample {
    pub start_ticks: u64,
    cpu_ticks: u64,
    rss_bytes: u64,
    ticks_per_second: u64,
}

#[cfg(target_os = "linux")]
pub(super) fn sample(pid: u32) -> Option<Sample> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(format!("/proc/{pid}/stat"))
        .ok()?
        .take(8192)
        .read_to_string(&mut text)
        .ok()?;
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if ticks <= 0 || page <= 0 {
        return None;
    }
    parse_stat(&text, ticks as u64, page as u64)
}

#[cfg(not(target_os = "linux"))]
pub(super) fn sample(_pid: u32) -> Option<Sample> {
    None
}

#[cfg(any(target_os = "linux", test))]
fn parse_stat(text: &str, ticks_per_second: u64, page_bytes: u64) -> Option<Sample> {
    // comm can contain spaces and parentheses; the final ')' ends field 2.
    let rest = text.rsplit_once(')')?.1;
    let fields: Vec<_> = rest.split_ascii_whitespace().collect();
    let number = |index: usize| fields.get(index)?.parse::<u64>().ok();
    Some(Sample {
        cpu_ticks: number(11)?.checked_add(number(12)?)?,
        start_ticks: number(19)?,
        rss_bytes: number(21)?.checked_mul(page_bytes)?,
        ticks_per_second,
    })
}

#[derive(Default)]
pub(super) struct Sampler {
    previous: HashMap<String, (u32, Sample, Instant)>,
}

impl Sampler {
    pub fn retain(&mut self, targets: &[String]) {
        self.previous.retain(|key, _| targets.contains(key));
    }

    pub fn observe(&mut self, target: &str, pid: u32) -> Value {
        let now = Instant::now();
        let Some(current) = sample(pid) else {
            self.previous.remove(target);
            return json!({"available":false,"cpu_percent_one_core":null,"rss_bytes":null,
                "scope":"client_process_only","reason":"process measurements unavailable"});
        };
        let cpu = self
            .previous
            .insert(target.into(), (pid, current, now))
            .and_then(|(old_pid, old, before)| {
                let seconds = now.duration_since(before).as_secs_f64();
                if old_pid != pid || old.start_ticks != current.start_ticks || seconds <= 0.0 {
                    return None;
                }
                current
                    .cpu_ticks
                    .checked_sub(old.cpu_ticks)
                    .map(|ticks| 100.0 * ticks as f64 / current.ticks_per_second as f64 / seconds)
                    .filter(|value| value.is_finite())
            });
        json!({"available":true,"cpu_percent_one_core":cpu,"rss_bytes":current.rss_bytes,
            "process_start_ticks":current.start_ticks.to_string(),"scope":"client_process_only",
            "cpu_state":if cpu.is_some(){"measured"}else{"sampling"},
            "source":"linux_proc_stat","descendants_included":false})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proc_fields_keep_parentheses_and_units_exact() {
        let mut fields = vec!["0"; 22];
        fields[0] = "S";
        fields[11] = "120";
        fields[12] = "30";
        fields[19] = "987654";
        fields[21] = "42";
        let text = format!("12 (command ) with spaces) {}", fields.join(" "));
        let value = parse_stat(&text, 100, 4096).unwrap();
        assert_eq!(value.cpu_ticks, 150);
        assert_eq!(value.start_ticks, 987654);
        assert_eq!(value.rss_bytes, 42 * 4096);
        assert!(parse_stat("12 malformed", 100, 4096).is_none());
    }
}
