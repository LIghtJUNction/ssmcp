#![cfg(target_os = "linux")]

pub mod backend;
pub mod broker;
pub mod client;
pub mod code_mode;
pub mod http;
pub mod identity;
pub mod oauth;
pub mod pty;
pub mod wire;
pub mod resources;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
pub mod workspace;
pub mod rsync;
pub mod runtime;
