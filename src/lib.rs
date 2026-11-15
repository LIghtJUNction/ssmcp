#![cfg(target_os = "linux")]

pub mod backend;
pub mod broker;
pub mod client;
pub mod http;
pub mod pty;
pub mod wire;

pub type Error = Box<dyn std::error::Error + Send + Sync>;
pub type Result<T> = std::result::Result<T, Error>;
