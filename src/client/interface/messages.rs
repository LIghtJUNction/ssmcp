//! Compatibility paths for the renderer and conversation history.
//! Native replies, presentations, retry receipts, and /copy share one owner.
pub(crate) use crate::client::messages::{Kind, remember_last, restore_copy};
