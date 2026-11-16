pub mod backend;
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
pub mod program;
#[cfg(target_os = "linux")]
pub mod portal;
#[cfg(target_os = "linux")]
pub mod pty;
#[cfg(not(target_os = "linux"))]
#[path = "pty_portable.rs"]
pub mod pty;
#[cfg(target_os = "linux")]
pub mod resources;
pub mod wire;
pub mod workspace;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
#[cfg(target_os = "linux")]
pub mod bridge;
#[cfg(target_os = "linux")]
pub mod rsync;
pub mod runtime;

#[cfg(target_os = "linux")]
pub mod observability;
