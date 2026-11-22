use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

const CAPACITY: usize = 8;
const MAX_DELTA: i32 = 128;
static REQUESTS: OnceLock<Mutex<VecDeque<i32>>> = OnceLock::new();

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Command {
    List,
    Delta(i32),
}

pub(super) fn parse(arguments: &str) -> Result<Command, String> {
    let arguments = arguments.trim();
    if arguments == "ls" {
        return Ok(Command::List);
    }
    let (sign, digits) = match arguments.as_bytes().first() {
        Some(b'+') => (1, &arguments[1..]),
        Some(b'-') => (-1, &arguments[1..]),
        _ => return Err("Usage: /worker ls | /worker +N | /worker -N.".into()),
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
    let mut requests = REQUESTS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "The worker request queue is unavailable.".to_owned())?;
    if requests.len() >= CAPACITY {
        return Err(
            "Worker changes are still pending. Try again after the current change finishes.".into(),
        );
    }
    requests.push_back(delta);
    Ok(())
}

pub(super) fn take() -> Option<i32> {
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
    fn accepts_explicit_worker_count_changes() {
        assert_eq!(parse("ls"), Ok(Command::List));
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
}
