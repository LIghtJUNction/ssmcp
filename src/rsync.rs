//! System rsync transport. Linux uses descriptor-rooted endpoints.
#[cfg(target_os = "linux")]
#[path = "rsync_linux.rs"]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::*;

#[cfg(not(target_os = "linux"))]
mod portable {
    use serde_json::Value;
    use std::path::PathBuf;
    pub struct Rsync;
    impl Rsync {
        pub fn new(_root: PathBuf) -> Result<Self, String> {
            Ok(Self)
        }
        pub fn tools() -> Vec<Value> {
            Vec::new()
        }
        pub async fn perform(&self, _name: &str, _arguments: Value) -> Result<Value, String> {
            Err("native rsync endpoints currently require Linux; use the native filesystem primitives on this platform".into())
        }
        pub async fn shutdown(&self) {}
    }
    pub async fn transport_main() -> crate::Result<()> {
        Err("native rsync transport currently requires Linux".into())
    }
}
#[cfg(not(target_os = "linux"))]
pub use portable::*;
