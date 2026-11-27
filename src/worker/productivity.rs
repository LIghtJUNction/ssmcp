use serde::Serialize;

pub(crate) const TITLE: &str = "Worker calls/min (log)";
pub(crate) const WIDTH: u16 = 32;

#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct Productivity {
    pub(crate) members: usize,
    pub(crate) calls_per_minute: [u64; 10],
}

impl Productivity {
    pub(super) fn record(&mut self, calls: [u64; 10]) {
        self.members += 1;
        for (total, count) in self.calls_per_minute.iter_mut().zip(calls) {
            *total = total.saturating_add(count);
        }
    }

    pub(crate) fn plot(&self) -> Vec<String> {
        const LEVELS: usize = 4;
        let peak = self.calls_per_minute.iter().copied().max().unwrap_or(0);
        let scale = (peak.max(1) as f64).ln_1p();
        let mut cells = [[b' '; 19]; LEVELS];
        let mut previous = None;
        for (index, count) in self.calls_per_minute.iter().enumerate() {
            let y = ((LEVELS - 1) as f64 * (*count as f64).ln_1p() / scale).round() as usize;
            cells[y][index * 2] = b'*';
            if let Some(last) = previous {
                let (low, high) = (y.min(last), y.max(last));
                for row in &mut cells[low..=high] {
                    row[index * 2 - 1] = if low == high { b'-' } else { b'|' };
                }
            }
            previous = Some(y);
        }

        let mut lines = Vec::with_capacity(5);
        for (level, row) in cells.iter().enumerate().rev() {
            let tick = (scale * level as f64 / (LEVELS - 1) as f64)
                .exp_m1()
                .round() as u64;
            let label = match level {
                0 => "0".to_owned(),
                level if level == LEVELS - 1 && peak >= 1_000_000 => format!("{peak:.0e}"),
                level if level == LEVELS - 1 => peak.max(1).to_string(),
                _ if tick == 0 || tick >= peak => String::new(),
                _ if tick >= 1_000_000 => format!("{tick:.0e}"),
                _ => tick.to_string(),
            };
            lines.push(format!(
                "{label:>8} |{}",
                std::str::from_utf8(row).expect("plot is ASCII")
            ));
        }
        lines.push("         + -10m -------- now".into());
        lines
    }
}
