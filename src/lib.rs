pub(crate) mod audit;
pub mod backend;
pub mod computer;
#[cfg(target_os = "linux")]
pub mod broker;
pub mod client;
#[cfg(target_os = "linux")]
pub mod code_mode;
pub mod context;
#[cfg(target_os = "linux")]
pub mod http;
#[cfg(target_os = "linux")]
pub mod identity;
#[cfg(target_os = "linux")]
pub mod oauth;
pub mod output;
pub mod platform;
#[cfg(target_os = "linux")]
pub mod portal;
pub mod program;
#[cfg_attr(not(target_os = "linux"), path = "pty_portable.rs")]
pub mod pty;
pub mod registry;
#[cfg(target_os = "linux")]
pub mod resources;
pub mod wire;
pub mod workspace;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
#[cfg(target_os = "linux")]
pub mod bridge;
pub mod rsync;
pub mod runtime;

pub mod observability;
