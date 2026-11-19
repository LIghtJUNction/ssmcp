//! Best-effort desktop notices for one logical wait episode.
//!
//! A successful helper only acknowledges submission to the system. It does not
//! establish that a banner was visible, or that the user saw it. Keep one
//! Notifier per client interface; a timeout retry must reuse its episode key.
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{collections::VecDeque, future::Future, path::Path, time::Duration};
use tokio::{sync::Mutex, time::Instant};

const DEADLINE: Duration = Duration::from_secs(5);
const HELPER_DEADLINE: Duration = Duration::from_secs(3);
const COOLDOWN: Duration = Duration::from_secs(60);
const RECENT_KEYS: usize = 128;
const MAX_KEY_BYTES: usize = 512;
const MAX_TITLE_BYTES: usize = 512;
const MAX_BODY_BYTES: usize = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Status {
    Sent,
    Unavailable,
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Reason {
    Submitted,
    #[cfg(any(target_os = "linux", windows, test))]
    DesktopSessionUnavailable,
    HelperUnavailable,
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    UnsupportedPlatform,
    HelperFailed,
    TimedOut,
    DeliveryUnconfirmed,
    RateLimited,
    InvalidRequest,
    Busy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct Report {
    pub(crate) status: Status,
    pub(crate) reason: Reason,
    pub(crate) deduplicated: bool,
    /// Always unknown: submission is neither evidence of viewing nor of ignoring.
    pub(crate) user_seen: Option<bool>,
}

impl Report {
    fn new(status: Status, reason: Reason) -> Self {
        Self {
            status,
            reason,
            deduplicated: false,
            user_seen: None,
        }
    }
}

#[derive(Default)]
struct History {
    recent: VecDeque<([u8; 32], Report)>,
    last_attempt: Option<Instant>,
}

/// No polling task, model, browser, or persistent helper process is created.
#[derive(Default)]
pub(crate) struct Notifier {
    history: Mutex<History>,
}

impl Notifier {
    pub(crate) async fn notify_wait(&self, key: &str, title: &str, body: &str) -> Report {
        if !valid_text(title, body) {
            return Report::new(Status::Failed, Reason::InvalidRequest);
        }
        self.notify_using(key, || submit(title, body)).await
    }

    async fn notify_using<F, Fut>(&self, key: &str, send: F) -> Report
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Report>,
    {
        self.notify_until(key, Instant::now() + DEADLINE, send)
            .await
    }

    async fn notify_until<F, Fut>(&self, key: &str, until: Instant, send: F) -> Report
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Report>,
    {
        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            return Report::new(Status::Failed, Reason::InvalidRequest);
        }
        let Ok(mut history) = tokio::time::timeout_at(until, self.history.lock()).await else {
            return Report::new(Status::Failed, Reason::Busy);
        };
        let key: [u8; 32] = Sha256::digest(key.as_bytes()).into();
        if let Some((_, previous)) = history.recent.iter().find(|(id, _)| *id == key) {
            return Report {
                deduplicated: true,
                ..*previous
            };
        }
        if history
            .last_attempt
            .is_some_and(|last| last.elapsed() < COOLDOWN)
        {
            // A suppressed new episode may try after the cooldown. Previously
            // attempted episodes remain deduplicated regardless of the clock.
            return Report::new(Status::Unavailable, Reason::RateLimited);
        }
        if history.recent.len() == RECENT_KEYS {
            history.recent.pop_front();
        }
        history.last_attempt = Some(Instant::now());
        history.recent.push_back((
            key,
            Report::new(Status::Failed, Reason::DeliveryUnconfirmed),
        ));
        // Reserve before the first await. Cancellation may happen after the OS
        // accepted a notice, so an interrupted attempt must not be sent again.
        let report = tokio::time::timeout_at(until, send())
            .await
            .unwrap_or_else(|_| Report::new(Status::Failed, Reason::TimedOut));
        history.recent.back_mut().unwrap().1 = report;
        report
    }
}

fn valid_text(title: &str, body: &str) -> bool {
    !title.trim().is_empty()
        && title.len() <= MAX_TITLE_BYTES
        && !title.chars().any(char::is_control)
        && !body.trim().is_empty()
        && body.len() <= MAX_BODY_BYTES
        && !body
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
}

#[derive(Clone, Copy, Debug)]
enum Platform {
    #[cfg(any(target_os = "linux", test))]
    Linux,
    #[cfg(any(target_os = "macos", test))]
    Macos,
    #[cfg(any(windows, test))]
    Windows,
}

struct Command {
    program: &'static str,
    arguments: Vec<String>,
    input: Option<Vec<u8>>,
}

// The source is constant. Text is supplied as argv, never AppleScript source.
#[cfg(any(target_os = "macos", test))]
const APPLESCRIPT: &str = "on run argv\n display notification (item 2 of argv) with title (item 1 of argv)\n return \"submitted\"\nend run";

// -Command reparses trailing arguments, so Windows text travels as JSON stdin.
// A short-lived, passive native tray notice needs no application registration,
// shortcut, third-party module, focus activation, or persistent registry write.
#[cfg(any(windows, test))]
const POWERSHELL: &str = r#"$ErrorActionPreference='Stop'
$ProgressPreference='SilentlyContinue'
[Console]::InputEncoding=[Text.UTF8Encoding]::new($false)
$notice=ConvertFrom-Json ([Console]::In.ReadToEnd())
if(-not [Environment]::UserInteractive){[Console]::Write('unavailable');exit 0}
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$icon=[System.Windows.Forms.NotifyIcon]::new()
try{
 $icon.Icon=[System.Drawing.SystemIcons]::Information
 $icon.Text='ssmcp'
 $icon.Visible=$true
 $icon.ShowBalloonTip(5000,[string]$notice.title,[string]$notice.body,[System.Windows.Forms.ToolTipIcon]::None)
 $timer=[Diagnostics.Stopwatch]::StartNew()
 while($timer.ElapsedMilliseconds -lt 1200){[System.Windows.Forms.Application]::DoEvents();[Threading.Thread]::Sleep(20)}
 [Console]::Write('submitted')
}finally{$icon.Visible=$false;$icon.Dispose()}
"#;

fn command(platform: Platform, title: &str, body: &str) -> Command {
    match platform {
        #[cfg(any(target_os = "linux", test))]
        Platform::Linux => Command {
            program: "notify-send",
            arguments: [
                "--app-name=ssmcp",
                "--urgency=normal",
                "--expire-time=5000",
                "--hint=boolean:suppress-sound:true",
                "--print-id",
                "--",
            ]
            .into_iter()
            .map(str::to_owned)
            .chain([title.to_owned(), markup_text(body)])
            .collect(),
            input: None,
        },
        #[cfg(any(target_os = "macos", test))]
        Platform::Macos => Command {
            program: "osascript",
            arguments: ["-e", APPLESCRIPT, "--", title, body]
                .into_iter()
                .map(str::to_owned)
                .collect(),
            input: None,
        },
        #[cfg(any(windows, test))]
        Platform::Windows => Command {
            program: "powershell.exe",
            arguments: [
                "-NoProfile",
                "-NonInteractive",
                "-STA",
                "-WindowStyle",
                "Hidden",
                "-Command",
                POWERSHELL,
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            input: Some(
                serde_json::to_vec(&serde_json::json!({
                    "title": title,
                    "body": body,
                }))
                .expect("notification strings are serializable"),
            ),
        },
    }
}

#[cfg(any(target_os = "linux", test))]
fn markup_text(value: &str) -> String {
    // freedesktop notification bodies can interpret XML markup and image URLs.
    // Literal user text must not acquire a link, image, or formatting action.
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn receipt(platform: Platform, output: &[u8]) -> Report {
    let output = output.trim_ascii();
    match platform {
        #[cfg(any(target_os = "linux", test))]
        Platform::Linux => {
            if std::str::from_utf8(output)
                .ok()
                .and_then(|text| text.parse::<u32>().ok())
                .is_some_and(|id| id > 0)
            {
                Report::new(Status::Sent, Reason::Submitted)
            } else {
                Report::new(Status::Failed, Reason::HelperFailed)
            }
        }
        #[cfg(any(target_os = "macos", test))]
        Platform::Macos => {
            if output == b"submitted" {
                Report::new(Status::Sent, Reason::Submitted)
            } else {
                Report::new(Status::Failed, Reason::HelperFailed)
            }
        }
        #[cfg(any(windows, test))]
        Platform::Windows => match output {
            b"submitted" => Report::new(Status::Sent, Reason::Submitted),
            b"unavailable" => Report::new(Status::Unavailable, Reason::DesktopSessionUnavailable),
            _ => Report::new(Status::Failed, Reason::HelperFailed),
        },
    }
}

async fn submit(title: &str, body: &str) -> Report {
    #[cfg(target_os = "linux")]
    let platform = {
        if !["DBUS_SESSION_BUS_ADDRESS", "WAYLAND_DISPLAY", "DISPLAY"]
            .into_iter()
            .any(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
        {
            return Report::new(Status::Unavailable, Reason::DesktopSessionUnavailable);
        }
        Platform::Linux
    };
    #[cfg(target_os = "macos")]
    let platform = Platform::Macos;
    #[cfg(windows)]
    let platform = Platform::Windows;
    #[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
    {
        let _ = (title, body);
        return Report::new(Status::Unavailable, Reason::UnsupportedPlatform);
    }
    #[cfg(any(target_os = "linux", target_os = "macos", windows))]
    {
        let request = command(platform, title, body);
        let Ok(program) = crate::program::executable(request.program) else {
            return Report::new(Status::Unavailable, Reason::HelperUnavailable);
        };
        let Some(program) = program.to_str() else {
            return Report::new(Status::Unavailable, Reason::HelperUnavailable);
        };
        match crate::computer::run_helper(
            Path::new("."),
            program,
            &request.arguments,
            request.input.as_deref(),
            64,
            HELPER_DEADLINE,
        )
        .await
        {
            Ok(out) => receipt(platform, &out),
            // Helper diagnostics may echo the notification text. Return only a
            // fixed category; neither the text nor stderr is written to a log.
            Err(_) => Report::new(Status::Failed, Reason::HelperFailed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn platform_arguments_keep_untrusted_text_out_of_programs() {
        let title = "ssmcp ' \" 中文";
        let body = "$(touch should-not-exist); `command`\n<img src='file:///private'> & --help";
        let linux = command(Platform::Linux, title, body);
        assert_eq!(linux.program, "notify-send");
        assert_eq!(&linux.arguments[5], "--");
        assert_eq!(&linux.arguments[6], title);
        assert_eq!(&linux.arguments[7], &markup_text(body));
        assert!(!linux.arguments[7].contains("<img"));
        assert!(linux.input.is_none());

        let macos = command(Platform::Macos, title, body);
        assert_eq!(macos.arguments, ["-e", APPLESCRIPT, "--", title, body]);
        assert!(!macos.arguments[1].contains(body));

        let windows = command(Platform::Windows, title, body);
        assert_eq!(windows.arguments.last().unwrap(), POWERSHELL);
        assert!(!windows.arguments.iter().any(|arg| arg.contains(body)));
        let input: serde_json::Value = serde_json::from_slice(&windows.input.unwrap()).unwrap();
        assert_eq!(input["title"], title);
        assert_eq!(input["body"], body);
    }

    #[test]
    fn only_platform_receipts_confirm_submission_and_never_user_viewing() {
        for (platform, output) in [
            (Platform::Linux, b"17\n".as_slice()),
            (Platform::Macos, b"submitted\n".as_slice()),
            (Platform::Windows, b"submitted".as_slice()),
        ] {
            let report = receipt(platform, output);
            assert_eq!(report.status, Status::Sent);
            assert_eq!(report.user_seen, None);
            assert_eq!(receipt(platform, b"").status, Status::Failed);
            assert_eq!(receipt(platform, b"unexpected text").status, Status::Failed);
        }
        assert_eq!(receipt(Platform::Linux, b"0").status, Status::Failed);
        assert_eq!(
            receipt(Platform::Windows, b"unavailable").status,
            Status::Unavailable
        );
    }

    #[test]
    fn rejects_empty_oversized_and_control_text_without_changing_unicode() {
        assert!(valid_text("需要输入", "第一行\n第二行\t内容"));
        for (title, body) in [
            ("", "body"),
            ("title\n", "body"),
            ("title", "\0"),
            ("title", "\x1b[1m"),
        ] {
            assert!(!valid_text(title, body));
        }
        assert!(!valid_text(&"x".repeat(MAX_TITLE_BYTES + 1), "body"));
        assert!(!valid_text("title", &"x".repeat(MAX_BODY_BYTES + 1)));
    }

    #[tokio::test]
    async fn repeated_wait_and_distinct_concurrent_calls_cannot_repeat_notices() {
        let notifier = Notifier::default();
        let calls = AtomicUsize::new(0);
        let send = || async {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::task::yield_now().await;
            Report::new(Status::Sent, Reason::Submitted)
        };
        let (first, repeated) = tokio::join!(
            notifier.notify_using("episode-1", send),
            notifier.notify_using("episode-1", send),
        );
        assert_eq!(first.status, Status::Sent);
        assert_eq!(repeated.status, Status::Sent);
        assert_ne!(first.deduplicated, repeated.deduplicated);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let blocked = notifier.notify_using("episode-2", send).await;
        assert_eq!(blocked.reason, Reason::RateLimited);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            serde_json::to_value(first).unwrap()["user_seen"],
            serde_json::Value::Null
        );
    }

    #[tokio::test]
    async fn cancellation_keeps_unknown_delivery_and_does_not_retry() {
        let notifier = Notifier::default();
        let calls = AtomicUsize::new(0);
        let attempt = notifier.notify_using("cancelled", || async {
            calls.fetch_add(1, Ordering::SeqCst);
            std::future::pending::<Report>().await
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(10), attempt)
                .await
                .is_err()
        );
        let report = notifier
            .notify_using("cancelled", || async {
                calls.fetch_add(1, Ordering::SeqCst);
                Report::new(Status::Sent, Reason::Submitted)
            })
            .await;
        assert_eq!(report.reason, Reason::DeliveryUnconfirmed);
        assert!(report.deduplicated);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn helper_and_lock_waits_share_a_bounded_deadline() {
        let notifier = Notifier::default();
        let report = notifier
            .notify_until(
                "stalled",
                Instant::now() + Duration::from_millis(10),
                std::future::pending,
            )
            .await;
        assert_eq!(report.reason, Reason::TimedOut);
        assert_eq!(
            notifier
                .notify_using("stalled", || async {
                    panic!("timed-out attempts must not be repeated")
                })
                .await
                .reason,
            Reason::TimedOut
        );
        let _held = notifier.history.lock().await;
        assert_eq!(
            notifier
                .notify_until(
                    "queued",
                    Instant::now() + Duration::from_millis(10),
                    || async { panic!("a blocked request must not start") },
                )
                .await
                .reason,
            Reason::Busy
        );
    }

    #[tokio::test]
    async fn failed_or_unavailable_submissions_are_not_automatically_retried() {
        for original in [
            Report::new(Status::Unavailable, Reason::HelperUnavailable),
            Report::new(Status::Failed, Reason::HelperFailed),
        ] {
            let notifier = Notifier::default();
            assert_eq!(
                notifier
                    .notify_using("episode", || async { original })
                    .await,
                original
            );
            let repeated = notifier
                .notify_using("episode", || async { panic!("must not retry") })
                .await;
            assert_eq!(repeated.reason, original.reason);
            assert!(repeated.deduplicated);
        }
    }

    #[tokio::test]
    async fn deduplication_retains_only_bounded_hashes_and_rate_limited_calls_can_retry() {
        let notifier = Notifier::default();
        for i in 0..RECENT_KEYS + 2 {
            notifier.history.lock().await.last_attempt = None;
            notifier
                .notify_using(&format!("episode-{i}"), || async {
                    Report::new(Status::Sent, Reason::Submitted)
                })
                .await;
        }
        assert_eq!(notifier.history.lock().await.recent.len(), RECENT_KEYS);
        assert_eq!(
            notifier
                .notify_using("next", || async { panic!("cooldown") })
                .await
                .reason,
            Reason::RateLimited
        );
        notifier.history.lock().await.last_attempt = Some(Instant::now() - COOLDOWN);
        assert_eq!(
            notifier
                .notify_using("next", || async {
                    Report::new(Status::Sent, Reason::Submitted)
                })
                .await
                .status,
            Status::Sent
        );
    }

    #[tokio::test]
    #[ignore = "submits one real desktop notice; requires SSMCP_NOTIFICATION_SMOKE=1"]
    async fn desktop_notification_smoke() {
        assert_eq!(
            std::env::var("SSMCP_NOTIFICATION_SMOKE").as_deref(),
            Ok("1")
        );
        let notifier = Notifier::default();
        let title = "ssmcp";
        let body = "System notification test. No response or action is required.";
        let report = notifier
            .notify_wait("owned-notification-smoke", title, body)
            .await;
        println!(
            "SSMCP_NOTIFICATION_REPORT {}",
            serde_json::to_string(&report).unwrap()
        );
        assert_eq!(report.status, Status::Sent);
        assert_eq!(report.user_seen, None);
        let repeated = notifier
            .notify_wait("owned-notification-smoke", title, body)
            .await;
        assert_eq!(repeated.status, Status::Sent);
        assert!(repeated.deduplicated);
    }
}
