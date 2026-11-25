use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

const CAPACITY: usize = 8;
const MAX_DELTA: i32 = 128;
#[derive(Debug, PartialEq, Eq, Clone)]
pub(super) struct Confirmation {
    pub request_id: u64,
    pub slot: u64,
    pub target: usize,
    pub generation: String,
    pub client_id: String,
}

impl Confirmation {
    pub fn matches(&self, snapshot: &serde_json::Value) -> bool {
        snapshot["generation"].as_str() == Some(self.generation.as_str())
            && snapshot["client_id"].as_str() == Some(self.client_id.as_str())
            && snapshot["desired_total"].as_u64() == Some(self.target as u64)
            // An in-flight claim keeps the selected station in the working count.
            && snapshot["total"].as_u64() == (self.target as u64).checked_add(1)
            && snapshot["last_resize"]["accepted"].as_bool() == Some(true)
            && snapshot["last_resize"]["target"].as_u64() == Some(self.target as u64)
            && snapshot["last_resize"]["request_id"].as_u64() == Some(self.request_id)
            && snapshot["last_resize"]["confirm_slot"].as_u64() == Some(self.slot)
    }
}

pub(super) enum Request {
    Delta(i32),
    RetireOne(Option<Confirmation>),
}

#[derive(Default)]
struct Input {
    requests: VecDeque<Request>,
    offered: Option<Confirmation>,
}

impl Input {
    fn submit(&mut self, command: Command) -> Result<(), String> {
        if self.requests.len() >= CAPACITY {
            return Err(
                "Worker changes are still pending. Try again after the current change finishes."
                    .into(),
            );
        }
        // Capture consent on this physical submission. Input already queued
        // before a prompt was shown cannot retroactively confirm that prompt.
        let request = match command {
            Command::RetireOne => Request::RetireOne(self.offered.take()),
            Command::Delta(delta) => {
                self.offered = None;
                Request::Delta(delta)
            }
            Command::List => return Err("Listing workers does not resize the pool.".into()),
        };
        self.requests.push_back(request);
        Ok(())
    }
}

fn input() -> &'static Mutex<Input> {
    static STATE: OnceLock<Mutex<Input>> = OnceLock::new();
    STATE.get_or_init(Default::default)
}

pub(super) fn offer(ticket: Confirmation) {
    input()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .offered = Some(ticket);
}

pub(super) fn cancel_offer() {
    input()
        .lock()
        .unwrap_or_else(|err| err.into_inner())
        .offered = None;
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    List,
    RetireOne,
    Delta(i32),
}

pub(super) fn parse(arguments: &str) -> Result<Command, String> {
    let arguments = arguments.trim();
    if arguments == "ls" {
        return Ok(Command::List);
    }
    if arguments == "-" {
        return Ok(Command::RetireOne);
    }
    let (sign, digits) = match arguments.as_bytes().first() {
        Some(b'+') => (1, &arguments[1..]),
        Some(b'-') => (-1, &arguments[1..]),
        _ => return Err("Usage: /worker ls | /worker +N | /worker -N | /worker -.".into()),
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

pub(super) fn enqueue(command: Command) -> Result<(), String> {
    input()
        .lock()
        .map_err(|_| "The worker request queue is unavailable.".to_owned())?
        .submit(command)
}

pub(super) fn take() -> Option<Request> {
    input().lock().ok()?.requests.pop_front()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accepts_explicit_worker_count_changes() {
        assert_eq!(parse("ls"), Ok(Command::List));
        assert_eq!(parse(" - "), Ok(Command::RetireOne));
        assert_eq!(parse(" +3 "), Ok(Command::Delta(3)));
        assert_eq!(parse("-128"), Ok(Command::Delta(-128)));
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
    #[test]
    fn only_a_fresh_local_submission_after_notice_can_confirm_once() {
        let mut ui = Input::default();
        let ticket = Confirmation {
            request_id: 7,
            slot: 4,
            target: 3,
            generation: "generation".into(),
            client_id: "client".into(),
        };
        ui.submit(Command::RetireOne).unwrap();
        ui.submit(Command::RetireOne).unwrap();
        ui.offered = Some(ticket.clone());
        assert!(matches!(
            ui.requests.pop_front(),
            Some(Request::RetireOne(None))
        ));
        assert!(matches!(
            ui.requests.pop_front(),
            Some(Request::RetireOne(None))
        ));
        ui.submit(Command::RetireOne).unwrap();
        ui.submit(Command::RetireOne).unwrap();
        assert!(
            matches!(ui.requests.pop_front(),Some(Request::RetireOne(Some(value))) if value==ticket)
        );
        assert!(matches!(
            ui.requests.pop_front(),
            Some(Request::RetireOne(None))
        ));
        ui.offered = Some(ticket);
        ui.submit(Command::Delta(-1)).unwrap();
        assert!(ui.offered.is_none());
    }

    #[test]
    fn confirmation_requires_the_displayed_pool_and_retirement() {
        let ticket = Confirmation {
            request_id: 7,
            slot: 4,
            target: 3,
            generation: "generation".into(),
            client_id: "client".into(),
        };
        let state = serde_json::json!({"generation":"generation","client_id":"client","desired_total":3,"total":4,"working":1,"retiring":0,
                                     "last_resize":{"request_id":7,"confirm_slot":4,"target":3,"accepted":true}});
        assert!(ticket.matches(&state));
        let mut idle = state.clone();
        idle["retiring"] = serde_json::json!(1);
        assert!(ticket.matches(&idle));
        for (key, value) in [
            ("generation", serde_json::json!("other")),
            ("client_id", serde_json::json!("other")),
            ("desired_total", serde_json::json!(2)),
            ("total", serde_json::json!(3)),
            (
                "last_resize",
                serde_json::json!({"request_id":8,"confirm_slot":4,"target":3,"accepted":true}),
            ),
        ] {
            let mut changed = state.clone();
            changed[key] = value;
            assert!(!ticket.matches(&changed));
        }
        for (key, value) in [
            ("confirm_slot", serde_json::json!(2)),
            ("target", serde_json::json!(1)),
            ("accepted", serde_json::json!(false)),
        ] {
            let mut changed = state.clone();
            changed["last_resize"][key] = value;
            assert!(!ticket.matches(&changed));
        }
    }
}
