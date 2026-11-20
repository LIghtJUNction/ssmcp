//! Question history is bound once at creation, independently of UI selection.
use super::*;
use crate::client::interface::history::{self as saved, HistoryOwner};

pub(super) type Owner = Result<HistoryOwner, String>;

pub(super) fn capture() -> Owner {
    crate::runtime::call_scope::Source::capture()
        .require_owner()
        .cloned()
}

pub(super) fn bytes(owner: &Option<Owner>) -> usize {
    owner.as_ref().map_or(0, |owner| {
        64 + match owner {
            Ok(owner) => owner.id().len(),
            Err(error) => error.len(),
        }
    })
}

pub(crate) fn owner(id: &str) -> Owner {
    state()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .records
        .iter()
        .find(|record| record.question.id == id)
        .ok_or("question is no longer retained")?
        .history_owner
        .clone()
        .ok_or_else(|| {
            "question_history_unavailable: the original history was not captured".to_owned()
        })?
}

pub(super) fn record_created(question: &Question) -> Result<(), String> {
    let owner = owner(&question.id)?;
    saved::record_owned_agent(&owner, None, &history_text(question)).map(|_| ())
}

#[cfg(test)]
pub(super) mod tests;
