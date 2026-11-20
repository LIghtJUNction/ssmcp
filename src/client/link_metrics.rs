//! Application-payload telemetry for this relay connection only.
//! Samples travel over a private local datagram socket, never the public wire.
use std::{
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, Instant},
};
use tokio::{process::Command, sync::watch, task::JoinHandle};

#[cfg(unix)]
#[path = "link_metrics/unix.rs"]
mod transport;
#[cfg(not(unix))]
#[path = "link_metrics/portable.rs"]
mod transport;
pub(super) use transport::Prepared;

const INTERVAL: Duration = Duration::from_secs(1);
#[cfg(unix)]
const STALE_AFTER: Duration = Duration::from_secs(4);
pub(super) const ENV: &str = "SSMCP_RELAY_METRICS_FD";
pub(super) const ID_ENV: &str = "SSMCP_RELAY_METRICS_SOCKET";
static GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum Status {
    Sampling,
    // Portable transports report Unavailable; Unix and unit tests produce samples.
    #[cfg_attr(not(any(unix, test)), allow(dead_code))]
    Ready {
        uploaded: u64,
        downloaded: u64,
    },
    #[default]
    Unavailable,
    Disconnected,
}

#[derive(Default)]
pub(super) struct View {
    generation: u64,
    client_id: String,
    status: Status,
}
impl View {
    pub(super) fn begin(&mut self, generation: u64, client_id: &str, status: Status) {
        if generation <= self.generation {
            return;
        }
        self.generation = generation;
        self.client_id = client_id
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .take(8)
            .collect();
        self.status = status;
    }

    pub(super) fn update(&mut self, generation: u64, status: Status) -> bool {
        if generation != self.generation || self.status == status {
            return false;
        }
        self.status = status;
        true
    }

    pub(super) fn rows(&self) -> [String; 3] {
        let id = if self.client_id.is_empty() {
            "—"
        } else {
            &self.client_id
        };
        let heading = format!("App data #{id}");
        match self.status {
            Status::Ready {
                uploaded,
                downloaded,
            } => [
                heading,
                format!("↑ {}", rate(uploaded)),
                format!("↓ {}", rate(downloaded)),
            ],
            status => [
                heading,
                match status {
                    Status::Sampling => "Sampling…",
                    Status::Unavailable => "Speed unavailable",
                    Status::Disconnected => "Disconnected",
                    Status::Ready { .. } => unreachable!(),
                }
                .into(),
                String::new(),
            ],
        }
    }
}

fn rate(bytes: u64) -> String {
    const UNITS: [&str; 7] = ["B/s", "KB/s", "MB/s", "GB/s", "TB/s", "PB/s", "EB/s"];
    let mut unit = 0;
    let mut divisor = 1u64;
    while unit + 1 < UNITS.len() && bytes / divisor >= 1000 {
        divisor *= 1000;
        unit += 1;
    }
    let whole = bytes / divisor;
    let tenth = u128::from(bytes % divisor) * 10 / u128::from(divisor);
    if unit != 0 && whole < 100 && tenth != 0 {
        format!("{whole}.{tenth} {}", UNITS[unit])
    } else {
        format!("{whole} {}", UNITS[unit])
    }
}

pub(super) fn prepare(command: &mut Command) -> Option<Prepared> {
    transport::prepare(command)
}

pub(super) struct Observer {
    generation: u64,
    task: Option<JoinHandle<()>>,
}
impl Drop for Observer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
        super::terminal::update_link_metrics(self.generation, Status::Disconnected);
    }
}

pub(super) fn observe(
    prepared: Option<Prepared>,
    client_id: &str,
    closed: watch::Receiver<Option<String>>,
) -> Option<Observer> {
    if !super::terminal::active() {
        return None;
    }
    let generation = GENERATION
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .ok()?;
    let status = if closed.borrow().is_some() {
        Status::Disconnected
    } else if prepared.is_some() {
        Status::Sampling
    } else {
        Status::Unavailable
    };
    super::terminal::begin_link_metrics(generation, client_id, status);
    let task = Some(transport::observe(prepared, closed, move |status| {
        super::terminal::update_link_metrics(generation, status);
    }));
    Some(Observer { generation, task })
}

async fn wait_closed(mut closed: watch::Receiver<Option<String>>) {
    loop {
        if closed.borrow().is_some() {
            return;
        }
        if closed.changed().await.is_err() {
            return;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Sample {
    elapsed_ms: u64,
    uploaded: u64,
    downloaded: u64,
}
impl Sample {
    const MAGIC: [u8; 8] = *b"SSMCPM01";
    fn encode(self) -> [u8; 32] {
        let mut bytes = [0; 32];
        bytes[..8].copy_from_slice(&Self::MAGIC);
        bytes[8..16].copy_from_slice(&self.elapsed_ms.to_be_bytes());
        bytes[16..24].copy_from_slice(&self.uploaded.to_be_bytes());
        bytes[24..32].copy_from_slice(&self.downloaded.to_be_bytes());
        bytes
    }
    #[cfg(any(unix, test))]
    fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() != 32 || bytes[..8] != Self::MAGIC {
            return None;
        }
        Some(Self {
            elapsed_ms: u64::from_be_bytes(bytes[8..16].try_into().ok()?),
            uploaded: u64::from_be_bytes(bytes[16..24].try_into().ok()?),
            downloaded: u64::from_be_bytes(bytes[24..32].try_into().ok()?),
        })
    }
}

#[cfg(any(unix, test))]
#[derive(Default)]
struct Sampler {
    previous: Option<Sample>,
    smooth: Option<(u64, u64)>,
}
#[cfg(any(unix, test))]
impl Sampler {
    fn observe(&mut self, sample: Sample) -> Status {
        let previous = self.previous.replace(sample);
        let rates = previous.and_then(|previous| {
            let elapsed = sample.elapsed_ms.checked_sub(previous.elapsed_ms)?;
            if !(500..=10_000).contains(&elapsed) {
                return None;
            }
            let rate = |now: u64, before: u64| {
                u64::try_from(u128::from(now.checked_sub(before)?) * 1000 / u128::from(elapsed))
                    .ok()
            };
            Some((
                rate(sample.uploaded, previous.uploaded)?,
                rate(sample.downloaded, previous.downloaded)?,
            ))
        });
        let Some((mut uploaded, mut downloaded)) = rates else {
            self.smooth = None;
            return Status::Sampling;
        };
        if let Some((before_up, before_down)) = self.smooth {
            uploaded = ((u128::from(before_up) + u128::from(uploaded)) / 2) as u64;
            downloaded = ((u128::from(before_down) + u128::from(downloaded)) / 2) as u64;
        }
        self.smooth = Some((uploaded, downloaded));
        Status::Ready {
            uploaded,
            downloaded,
        }
    }
}

pub(super) struct Recorder {
    reporter: Option<transport::Reporter>,
    started: Instant,
    last_report: Option<Instant>,
    uploaded: u64,
    downloaded: u64,
}
impl Recorder {
    pub(super) fn from_environment() -> Self {
        Self::new(transport::reporter())
    }

    fn new(reporter: Option<transport::Reporter>) -> Self {
        Self {
            reporter,
            started: Instant::now(),
            last_report: None,
            uploaded: 0,
            downloaded: 0,
        }
    }

    pub(super) fn enabled(&self) -> bool {
        self.reporter.is_some()
    }

    pub(super) fn sent(&mut self, bytes: usize) {
        self.add(bytes, true);
    }

    pub(super) fn received(&mut self, bytes: usize) {
        self.add(bytes, false);
    }

    fn add(&mut self, bytes: usize, upload: bool) {
        let amount = bytes as u64;
        let value = if upload {
            self.uploaded
        } else {
            self.downloaded
        };
        if let Some(next) = value.checked_add(amount) {
            if upload {
                self.uploaded = next;
            } else {
                self.downloaded = next;
            }
        } else {
            // A new baseline is safer than saturating into a false rate.
            self.started = Instant::now();
            self.uploaded = if upload { amount } else { 0 };
            self.downloaded = if upload { 0 } else { amount };
        }
    }

    pub(super) fn publish(&mut self) {
        let now = Instant::now();
        if self
            .last_report
            .is_some_and(|last| now.duration_since(last) < INTERVAL)
        {
            return;
        }
        let Some(reporter) = self.reporter.as_ref() else {
            return;
        };
        self.last_report = Some(now);
        let sample = Sample {
            elapsed_ms: u64::try_from(now.duration_since(self.started).as_millis())
                .unwrap_or(u64::MAX),
            uploaded: self.uploaded,
            downloaded: self.downloaded,
        };
        if let Err(error) = reporter.send(&sample.encode())
            && error.kind() != std::io::ErrorKind::WouldBlock
        {
            // No subscriber must never hold up the WebSocket pump.
            self.reporter = None;
        }
    }

    #[cfg(test)]
    pub(super) fn disabled() -> Self {
        Self::new(None)
    }

    #[cfg(test)]
    pub(super) fn totals(&self) -> (u64, u64) {
        (self.uploaded, self.downloaded)
    }
}

#[cfg(test)]
mod tests;
