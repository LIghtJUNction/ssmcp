use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const CAPACITY: usize = 8;
const MAX_DELTA: i32 = 128;
const CONFIRMATION_TTL: Duration = Duration::from_secs(30);
static REQUESTS: OnceLock<Mutex<VecDeque<Request>>> = OnceLock::new();

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Confirmation {
    pub request_id: u64,
    pub slot: usize,
    pub generation: String,
    pub client_id: String,
    pub task_id: Option<String>,
    expires_at: Instant,
}

impl Confirmation {
    pub fn from_preview(
        request_id: u64,
        slot: usize,
        snapshot: &serde_json::Value,
    ) -> Option<Self> {
        if request_id == 0 || slot >= MAX_DELTA as usize {
            return None;
        }
        let task_id = match &snapshot["last_resize"]["task_id"] {
            serde_json::Value::Null => None,
            serde_json::Value::String(task)
                if !task.is_empty() && !task.chars().any(char::is_control) =>
            {
                Some(task.clone())
            }
            _ => return None,
        };
        let confirmation = Self {
            request_id,
            slot,
            task_id,
            generation: snapshot["generation"].as_str()?.to_owned(),
            client_id: snapshot["client_id"].as_str()?.to_owned(),
            expires_at: Instant::now() + CONFIRMATION_TTL,
        };
        confirmation.matches(snapshot).then_some(confirmation)
    }

    pub fn fresh(&self, now: Instant) -> bool {
        now < self.expires_at
    }

    pub fn matches(&self, snapshot: &serde_json::Value) -> bool {
        let receipt = &snapshot["last_resize"];
        self.fresh(Instant::now())
            && !self.generation.is_empty()
            && !self.client_id.is_empty()
            && snapshot["generation"].as_str() == Some(self.generation.as_str())
            && snapshot["client_id"].as_str() == Some(self.client_id.as_str())
            && receipt["accepted"].as_bool() == Some(true)
            && receipt["request_id"].as_u64() == Some(self.request_id)
            && receipt["confirm_slot"].as_u64() == Some(self.slot as u64 + 1)
            && match &self.task_id {
                Some(task) => receipt["task_id"].as_str() == Some(task.as_str()),
                None => receipt["task_id"].is_null(),
            }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ResizeProgress {
    pub target: usize,
    total: usize,
    awaiting_release: usize,
}

impl ResizeProgress {
    pub fn read(state: &serde_json::Value) -> Option<Self> {
        Some(Self {
            target: state["desired_total"].as_u64()? as usize,
            total: state["total"].as_u64()? as usize,
            awaiting_release: state["retiring"].as_u64()? as usize,
        })
    }

    pub fn settled(self) -> bool {
        self.total == self.target && self.awaiting_release == 0
    }

    pub fn notice(self) -> String {
        if self.settled() {
            return format!("Worker capacity is now {}.", self.total);
        }
        let finishing = self
            .total
            .saturating_sub(self.target + self.awaiting_release);
        format!(
            "Worker target {} · {} stations · {} finishing claimed work · {} awaiting release.",
            self.target, self.total, finishing, self.awaiting_release
        )
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Request {
    Delta(i32),
    Retire {
        slot: usize,
        confirmation: Option<Confirmation>,
    },
}

#[derive(Debug)]
pub(super) struct Prompt {
    pub confirmation: Confirmation,
    presented_at: Option<Instant>,
}

impl Prompt {
    pub fn new(confirmation: Confirmation) -> Self {
        Self {
            confirmation,
            presented_at: None,
        }
    }

    pub fn present(&mut self, rendered: &Confirmation, now: Instant) {
        if self.confirmation == *rendered && rendered.fresh(now) {
            self.presented_at.get_or_insert(now);
        }
    }

    pub fn submission(&self, slot: usize, received_at: Instant) -> Option<Confirmation> {
        (self.confirmation.slot == slot
            && self.confirmation.fresh(Instant::now())
            && self.presented_at.is_some_and(|shown| shown < received_at))
        .then(|| self.confirmation.clone())
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    List,
    Retire(usize),
    Delta(i32),
}

pub(super) fn parse(arguments: &str) -> Result<Command, String> {
    let arguments = arguments.trim();
    if arguments == "ls" {
        return Ok(Command::List);
    }
    if let Some((verb, number)) = arguments.split_once(char::is_whitespace)
        && verb == "retire"
    {
        let number = number.trim_start();
        let digits = number.strip_prefix('#').unwrap_or(number);
        let slot = digits
            .parse::<usize>()
            .ok()
            .filter(|n| {
                !digits.is_empty()
                    && digits.bytes().all(|b| b.is_ascii_digit())
                    && (1..=MAX_DELTA as usize).contains(n)
            })
            .ok_or("Use /worker retire #N with a station number from 1 to 128.")?;
        return Ok(Command::Retire(slot - 1));
    }
    let (sign, digits) = match arguments.as_bytes().first() {
        Some(b'+') => (1, &arguments[1..]),
        Some(b'-') => (-1, &arguments[1..]),
        _ => return Err("Usage: /worker ls | /worker +N | /worker -N | /worker retire #N.".into()),
    };
    if digits.is_empty() || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return Err("Use a whole worker count from 1 to 128.".into());
    }
    let amount = digits
        .parse::<i32>()
        .map_err(|_| "Use a whole worker count from 1 to 128.".to_owned())?;
    if !(1..=MAX_DELTA).contains(&amount) {
        return Err("Use a whole worker count from 1 to 128.".into());
    }
    Ok(Command::Delta(sign * amount))
}

pub(super) fn enqueue_delta(delta: i32) -> Result<(), String> {
    if delta == 0 || !(-MAX_DELTA..=MAX_DELTA).contains(&delta) {
        return Err("Use a whole worker count from 1 to 128.".into());
    }
    enqueue(Request::Delta(delta))
}

pub(super) fn enqueue(request: Request) -> Result<(), String> {
    let mut requests = REQUESTS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "The worker request queue is unavailable.".to_owned())?;
    if requests.len() >= CAPACITY {
        return Err(
            "Worker changes are still pending. Try again after the current change finishes.".into(),
        );
    }
    requests.push_back(request);
    Ok(())
}

pub(super) fn take() -> Option<Request> {
    REQUESTS
        .get_or_init(Default::default)
        .lock()
        .ok()?
        .pop_front()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shrink_progress_includes_work_before_the_release_acknowledgement() {
        let mut state = serde_json::json!({
            "total": 6, "desired_total": 5, "retiring": 0, "offline": 0,
            "working": 6, "queued": 1, "activity": {"offline": 1}
        });
        let progress = ResizeProgress::read(&state).unwrap();
        assert!(!progress.settled());
        assert!(
            progress
                .notice()
                .contains("1 finishing claimed work · 0 awaiting release")
        );
        state["retiring"] = 1.into();
        let progress = ResizeProgress::read(&state).unwrap();
        assert!(!progress.settled());
        assert!(
            progress
                .notice()
                .contains("0 finishing claimed work · 1 awaiting release")
        );
        state["total"] = 5.into();
        state["retiring"] = 0.into();
        assert_eq!(
            ResizeProgress::read(&state).unwrap().notice(),
            "Worker capacity is now 5."
        );
    }

    #[test]
    fn accepts_explicit_worker_count_changes() {
        assert_eq!(parse("ls"), Ok(Command::List));
        assert_eq!(parse(" +3 "), Ok(Command::Delta(3)));
        assert_eq!(parse("-128"), Ok(Command::Delta(-128)));
    }
    #[test]
    fn parses_human_station_numbers_without_changing_count_semantics() {
        for input in ["retire #03", "retire 3", "retire    003"] {
            assert_eq!(parse(input), Ok(Command::Retire(2)));
        }
        assert_eq!(parse("retire #128"), Ok(Command::Retire(127)));
        for input in [
            "retire",
            "retire #0",
            "retire -1",
            "retire +3",
            "retire #129",
            "retire 3 4",
            "-",
        ] {
            assert!(parse(input).is_err(), "{input}");
        }
    }

    #[test]
    fn retirement_consent_is_bound_to_render_time_station_receipt_task_and_lifetime() {
        let mut snapshot = serde_json::json!({
            "client_id":"child", "generation":"pool", "last_resize":{
                "accepted":true, "request_id":9, "confirm_slot":3, "task_id":"job"
            }
        });
        let mut ticket = Confirmation::from_preview(9, 2, &snapshot).unwrap();
        let t = Instant::now();
        let mut prompt = Prompt::new(ticket.clone());
        assert!(prompt.submission(2, t).is_none());
        prompt.present(&ticket, t);
        assert!(prompt.submission(2, t).is_none());
        let next = t + Duration::from_millis(1);
        assert!(prompt.submission(1, next).is_none());
        assert_eq!(prompt.submission(2, next), Some(ticket.clone()));
        snapshot["generation"] = "replacement".into();
        assert!(!ticket.matches(&snapshot));
        snapshot["generation"] = "pool".into();
        snapshot["last_resize"]["task_id"] = "new-task".into();
        assert!(!ticket.matches(&snapshot));
        snapshot["last_resize"]["task_id"] = "job".into();
        assert!(ticket.matches(&snapshot));
        ticket.expires_at = Instant::now();
        assert!(!ticket.matches(&snapshot));
    }

    #[test]
    fn rejects_ambiguous_or_out_of_range_worker_counts() {
        for text in [
            "",
            "3",
            "+0",
            "-0",
            "+129",
            "-2147483648",
            "+1.5",
            "+ 3",
            "+3 more",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }
}
