//! Compatibility name for terminal callers. All jobs, selection and lifecycle
//! state are owned by the existing exec registry in Shell.
pub use crate::backend::shell::Shell as Terminals;
pub(crate) const MAX_TERMINALS: usize = 16;
