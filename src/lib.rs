pub(crate) mod audit;
pub mod backend;
#[cfg(target_os = "linux")]
pub mod broker;
pub mod browser;
pub(crate) mod catalog;
pub mod client;
#[cfg(target_os = "linux")]
pub mod code_mode;
#[cfg(target_os = "linux")]
mod collab;
pub mod computer;
pub mod context;
pub(crate) mod digest;
#[cfg(target_os = "linux")]
pub mod history;
#[cfg(target_os = "linux")]
pub mod http;
pub(crate) mod http_client;
#[cfg(target_os = "linux")]
pub mod identity;
#[cfg(target_os = "linux")]
pub(crate) mod local_socket;
#[cfg(target_os = "linux")]
pub mod management;
pub(crate) mod memory;
#[cfg(target_os = "linux")]
pub(crate) mod messaging;
#[cfg(target_os = "linux")]
pub mod oauth;
pub(crate) mod observer;
pub mod omarchy;
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
pub(crate) mod server_endpoint;
pub mod session;
#[cfg(target_os = "linux")]
mod steering;
pub mod terminals;
pub mod wire;
pub mod workspace;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
#[cfg(target_os = "linux")]
pub mod bridge;
pub mod rsync;
pub mod runtime;

pub mod observability;

#[cfg(target_os = "linux")]
pub mod device;

#[cfg(target_os = "linux")]
pub mod client_downloads;

#[cfg(target_os = "linux")]
mod ssh_public_key;

#[cfg(target_os = "linux")]
pub mod user_policy;
pub(crate) mod username;

#[cfg(target_os = "linux")]
pub mod commerce;

pub mod webhooks;
