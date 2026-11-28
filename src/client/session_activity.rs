//! Opt-in tool-call count for the desktop widget that owns this foreground CLI.
use crate::{tool_window::ToolWindow, wire::Operation};
use std::{
    io,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{task::JoinHandle, time::Instant};

pub(super) fn owner_path() -> Option<PathBuf> {
    let owner: u32 = std::env::var("SSMCP_SESSION_ACTIVITY_PID")
        .ok()?
        .parse()
        .ok()?;
    if std::process::id() != owner {
        return None;
    }
    std::env::var_os("SSMCP_SESSION_ACTIVITY_PATH")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.file_name().is_some())
}

pub(super) struct Activity {
    window: Arc<Mutex<ToolWindow>>,
    publisher: JoinHandle<()>,
}

impl Activity {
    pub(super) fn start(path: PathBuf) -> Self {
        let window = Arc::new(Mutex::new(ToolWindow::new(Instant::now())));
        let publisher = tokio::spawn(publish(window.clone(), path));
        Self { window, publisher }
    }

    pub(super) fn record(&self, operation: &Operation) {
        if matches!(operation, Operation::CallTool { .. })
            && let Ok(mut window) = self.window.lock()
        {
            window.record(Instant::now());
        }
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        self.publisher.abort();
    }
}

async fn publish(window: Arc<Mutex<ToolWindow>>, path: PathBuf) {
    let mut interval = tokio::time::interval(Duration::from_secs(1));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut previous = None;
    let temporary = path.with_extension("tmp");
    loop {
        interval.tick().await;
        let count = match window.lock() {
            Ok(window) => window.measure(Instant::now()).0,
            Err(_) => return,
        };
        if previous == Some(count) {
            continue;
        }
        let result = async {
            let snapshot = format!("{{\"tool_calls_10m\":{count}}}\n");
            tokio::fs::write(&temporary, snapshot).await?;
            tokio::fs::rename(&temporary, &path).await
        }
        .await;
        match result {
            Ok(()) => previous = Some(count),
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(_) => {}
        }
    }
}
