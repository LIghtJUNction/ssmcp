//! Display evidence belonging to one authenticated membership. A bind, lease
//! renewal, user wake or roster read does not enter this accounting.
use crate::wire::{MainActivity, MainPhase, Operation};
use tokio::time::Instant;

#[derive(Clone, Copy)]
pub(super) enum DisplayCall {
    Work,
    Wait,
}

impl DisplayCall {
    pub(super) fn classify(operation: &Operation) -> Option<Self> {
        let Operation::CallTool { name, arguments } = operation else {
            return None;
        };
        if crate::runtime::time::blocking_observation(name, arguments)
            || name == "ssmcp.ui.wait" && arguments["op"] == "wait"
        {
            return Some(Self::Wait);
        }
        match name.as_str() {
            "ssmcp.exec.start"
            | "ssmcp.exec.write"
            | "terminal.open"
            | "terminal.write"
            | "ssmcp.fs.read"
            | "ssmcp.fs.read_text"
            | "ssmcp.fs.read_image"
            | "ssmcp.fs.write"
            | "ssmcp.fs.write_text"
            | "ssmcp.fs.edit_text"
            | "ssmcp.fs.mkdir"
            | "ssmcp.skills.read"
            | "ssmcp.worker.submit"
            | "ssmcp.worker.complete" => Some(Self::Work),
            "ssmcp.ui.message" | "ssmcp.ui.present"
                if matches!(
                    arguments["kind"].as_str(),
                    None | Some("progress" | "answer")
                ) =>
            {
                Some(Self::Work)
            }
            _ => None,
        }
    }
}

pub(in crate::collab) struct MainObservation {
    joined: Instant,
    work: usize,
    waits: usize,
    observed: Option<Instant>,
    unknown: bool,
}

impl MainObservation {
    pub(in crate::collab) fn new(joined: Instant) -> Self {
        Self {
            joined,
            work: 0,
            waits: 0,
            observed: None,
            unknown: false,
        }
    }

    pub(super) fn start(&mut self, call: DisplayCall, now: Instant) {
        match call {
            DisplayCall::Wait => self.waits += 1,
            DisplayCall::Work => {
                self.work += 1;
                self.observed = Some(now);
                self.unknown = false;
            }
        }
    }

    pub(super) fn finish(&mut self, call: DisplayCall, confirmed: bool, now: Instant) {
        match call {
            DisplayCall::Wait => self.waits -= 1,
            DisplayCall::Work => {
                self.work -= 1;
                self.observed = Some(now);
                self.unknown = !confirmed;
            }
        }
    }

    pub(in crate::collab) fn snapshot(&self, now: Instant) -> MainActivity {
        let age = |at| {
            now.saturating_duration_since(at)
                .as_millis()
                .min(u128::from(u64::MAX)) as u64
        };
        MainActivity {
            connected_ms: age(self.joined),
            phase: if self.work > 0 {
                MainPhase::Work
            } else if self.waits > 0 {
                MainPhase::Wait
            } else if self.unknown {
                MainPhase::Unknown
            } else if self.observed.is_some() {
                MainPhase::Recent
            } else {
                MainPhase::Quiet
            },
            observed_age_ms: self.observed.map(age),
        }
    }
}
