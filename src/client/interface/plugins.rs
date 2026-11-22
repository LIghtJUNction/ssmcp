//! Explicit local management of the connected client's trusted native plugins.
//! Use the registered tools so their hooks, catalog generations and limits apply.
use crate::{runtime::call_scope::Source, wire::Operation};
use serde_json::{Value, json};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

pub(crate) const HELP: &str = "## Native plugins\n\n\
/plugin                  List plugins in this client\n\
/plugin list             List plugins and their current state\n\
/plugin load NAME PATH   Load one trusted local shared library\n\
/plugin unload NAME      Disable its tools and request its worker to stop\n\
/plugin help             Show this help\n\n\
NAME uses 1–32 ASCII letters, digits, _ or -. Quote a path containing spaces\n\
with single or double quotes. Backslashes remain literal; there is no shell,\n\
environment-variable or home-directory expansion. Relative paths use this\n\
client's workspace.\n\n\
Loading runs native code with this client's privileges. No package is downloaded\n\
or installed. Unload does not delete the file or forcibly stop a native call;\n\
loaded library images remain resident until this client process exits.\n\n\
This manages native plugins, not the JavaScript sources managed by ssmcp tools.\n";

const MAX_COMMAND: usize = 16 * 1024;
const MAX_PATH: usize = 4096;
const MAX_ACTIVE: usize = 4;
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, PartialEq)]
enum Action {
    Help,
    List,
    Load { namespace: String, path: String },
    Unload { namespace: String },
}

impl Action {
    fn label(&self) -> &'static str {
        match self {
            Self::Help => "help",
            Self::List => "list",
            Self::Load { .. } => "load",
            Self::Unload { .. } => "unload",
        }
    }

    fn operation(&self) -> Option<Operation> {
        let (name, arguments) = match self {
            Self::Help => return None,
            Self::List => ("ssmcp.plugins.list", json!({})),
            Self::Load { namespace, path } => (
                "ssmcp.plugins.load",
                json!({"namespace":namespace,"path":path}),
            ),
            Self::Unload { namespace } => ("ssmcp.plugins.unload", json!({"namespace":namespace})),
        };
        Some(Operation::CallTool {
            name: name.into(),
            arguments,
        })
    }
}

fn namespace(value: &str) -> Result<String, String> {
    if value.is_empty()
        || value.len() > 32
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
    {
        return Err("Plugin NAME must use 1–32 ASCII letters, digits, _ or -".into());
    }
    Ok(value.into())
}

fn path_argument(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    let path = if let Some(quote) = raw.chars().next().filter(|ch| matches!(*ch, '\'' | '"')) {
        raw.strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
            .filter(|inner| !inner.contains(quote))
            .ok_or("Use one complete quoted path after /plugin load NAME")?
    } else {
        if raw
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '\'' | '"'))
        {
            return Err(
                "Use one path after /plugin load NAME; quote paths containing spaces".into(),
            );
        }
        raw
    };
    if path.is_empty() || path.len() > MAX_PATH || path.chars().any(char::is_control) {
        return Err(
            "Plugin PATH must contain 1–4096 UTF-8 bytes without control characters".into(),
        );
    }
    // Do not interpret ~, $, backticks, backslashes or command substitutions.
    // Filesystem resolution and the trusted-code decision stay with Plugins::load.
    Ok(path.into())
}

fn parse(raw: &str) -> Result<Action, String> {
    if raw.len() > MAX_COMMAND || raw.chars().any(|ch| ch.is_control() && ch != '\t') {
        return Err("Plugin command exceeds 16384 bytes or contains control characters".into());
    }
    let raw = raw.trim();
    let (verb, rest) = raw
        .split_once(char::is_whitespace)
        .map_or((raw, ""), |(verb, rest)| (verb, rest.trim()));
    match (verb, rest) {
        ("", "") | ("list", "") => Ok(Action::List),
        ("help" | "-h" | "--help", "") => Ok(Action::Help),
        ("unload", rest) => Ok(Action::Unload {
            namespace: namespace(rest)?,
        }),
        ("load", rest) => {
            let (name, path) = rest
                .split_once(char::is_whitespace)
                .ok_or("Use /plugin load NAME PATH; quote paths containing spaces")?;
            Ok(Action::Load {
                namespace: namespace(name)?,
                path: path_argument(path)?,
            })
        }
        _ => Err("Use /plugin [list | load NAME PATH | unload NAME | help]".into()),
    }
}

struct Slot<'a>(&'a AtomicUsize);

impl<'a> Slot<'a> {
    fn acquire(counter: &'a AtomicUsize) -> Result<Self, String> {
        let mut current = counter.load(Ordering::Acquire);
        loop {
            if current >= MAX_ACTIVE {
                return Err("Four plugin commands are already running; wait for a result".into());
            }
            match counter.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(Self(counter)),
                Err(observed) => current = observed,
            }
        }
    }
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn json_message(heading: &str, value: &Value) -> Result<String, String> {
    let text = serde_json::to_string_pretty(value)
        .map_err(|error| format!("Cannot format plugin result: {error}"))?;
    // Indentation cannot be closed by a filename or native error containing ```.
    let block = text
        .lines()
        .map(|line| format!("    {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!("## {heading}\n\n{block}"))
}

fn result_message(action: &Action, request: u64, value: &Value) -> Result<String, String> {
    let data = value.get("structuredContent").unwrap_or(value);
    let status = if value["isError"] == true {
        "returned an error"
    } else if data["spilled"] == true {
        "result saved to an output artifact"
    } else {
        match action {
            Action::List => "current state",
            Action::Load { .. } if data["status"] == "ready" => "ready",
            Action::Unload { .. } if data["status"] == "stopping" => "stop requested",
            _ => "result",
        }
    };
    let heading = format!("/plugin {} · request {request} · {status}", action.label());
    let visible = if value["isError"] == true {
        value
    } else {
        data
    };
    let mut message = json_message(&heading, visible)?;
    if matches!(action, Action::List)
        && data["plugins"].as_array().is_some_and(Vec::is_empty)
        && value["isError"] != true
    {
        message.push_str(
            "\n\nNo native plugins are registered in this client. Use /plugin help for commands.",
        );
    }
    if matches!(action, Action::Unload { .. }) && value["isError"] != true {
        message.push_str("\n\nThis is a stop request, not forced cancellation or file removal. Check /plugin list for worker_running; native library images remain resident until process exit.");
    }
    Ok(message)
}

/// Queue one explicit local command; never wait or render while the UI lock is held.
/// The UI must reject live operations in preview mode and keep /plugin local even
/// when a native message recipient is selected. It can display HELP directly offline.
pub(crate) fn submit(raw: &str) -> Result<(), String> {
    let action = parse(raw)?;
    let source = Source::capture();
    let Some(operation) = action.operation() else {
        // No connected runtime, plugin scan or library loading is needed for help.
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| "Display /plugin help locally without starting a client")?;
        handle.spawn(source.scope(async {
            crate::client::render::message(HELP);
        }));
        return Ok(());
    };
    let (runtime, handle) = super::harness::local_runtime()?;
    runtime.local_registry().ensure_open()?;
    let slot = Slot::acquire(&ACTIVE)?;
    let request = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    handle.spawn(source.scope(async move {
        let _slot = slot;
        // Keep the existing runtime's hook checks, registry validation and native
        // timeouts. A timeout does not imply native code stopped; never retry it.
        let message = match runtime.perform(operation).await {
            Ok(value) => result_message(&action, request, &value),
            Err(error) => json_message(
                &format!("/plugin {} · request {request} · failed", action.label()),
                &json!({"error":error}),
            ),
        };
        match message {
            Ok(message) => crate::client::render::message(&message),
            Err(error) => crate::client::render::message(&format!("/plugin: {error}")),
        }
    }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(raw: &str) -> (String, Value) {
        let Some(Operation::CallTool { name, arguments }) = parse(raw).unwrap().operation() else {
            panic!("expected one native plugin-management call");
        };
        (name, arguments)
    }

    #[test]
    fn only_explicit_management_actions_reach_native_tools() {
        assert_eq!(call(""), ("ssmcp.plugins.list".into(), json!({})));
        assert_eq!(call("list"), call(""));
        assert_eq!(
            call("load demo ./libdemo.so"),
            (
                "ssmcp.plugins.load".into(),
                json!({"namespace":"demo","path":"./libdemo.so"})
            )
        );
        assert_eq!(
            call("unload demo"),
            ("ssmcp.plugins.unload".into(), json!({"namespace":"demo"}))
        );
        for raw in [
            "install demo",
            "update",
            "call plugins.demo.exec {}",
            "list extra",
            "help extra",
            "unload",
            "unload demo other",
            "load demo",
            "load ../bad library",
            "load 中文 library",
            "load demo library extra",
            "list\nload demo library",
            "list\u{1b}",
        ] {
            // Invalid input fails before consulting any live runtime binding.
            assert!(parse(raw).is_err(), "{raw:?}");
            assert!(submit(raw).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn paths_are_one_literal_argument_on_every_platform() {
        for (raw, expected) in [
            (r#"load demo './插件 library.so'"#, "./插件 library.so"),
            (
                r#"load demo "C:\Program Files\demo.dll""#,
                r"C:\Program Files\demo.dll",
            ),
            (r"load demo C:\plugins\demo.dll", r"C:\plugins\demo.dll"),
            (
                r#"load demo '$HOME/$(echo nope)/`literal`.so'"#,
                "$HOME/$(echo nope)/`literal`.so",
            ),
            ("load demo ~/demo.so", "~/demo.so"),
        ] {
            assert_eq!(call(raw).1["path"], expected);
        }
        for raw in [
            "load demo ''",
            "load demo 'unfinished",
            "load demo 'one' 'two'",
            "load demo \"one\"suffix",
            "load demo 'bad\tpath'",
        ] {
            assert!(parse(raw).is_err(), "{raw:?}");
        }
        assert!(parse(&format!("load {} path", "x".repeat(33))).is_err());
        assert!(parse(&format!("load demo {}", "x".repeat(MAX_PATH + 1))).is_err());
    }

    #[test]
    fn help_is_static_and_never_contains_a_native_operation() {
        for raw in ["help", "-h", "--help"] {
            assert_eq!(parse(raw).unwrap(), Action::Help);
            assert!(parse(raw).unwrap().operation().is_none());
        }
        assert!(HELP.contains("/plugin load NAME PATH"));
        assert!(HELP.contains("No package is downloaded"));
        assert!(HELP.contains("remain resident until this client process exits"));
    }

    #[test]
    fn pending_submission_slots_are_bounded_and_released_on_drop() {
        let active = AtomicUsize::new(0);
        let mut slots = (0..MAX_ACTIVE)
            .map(|_| Slot::acquire(&active).unwrap())
            .collect::<Vec<_>>();
        assert!(Slot::acquire(&active).is_err());
        drop(slots.pop());
        let replacement = Slot::acquire(&active).unwrap();
        assert_eq!(active.load(Ordering::Acquire), MAX_ACTIVE);
        drop(replacement);
        drop(slots);
        assert_eq!(active.load(Ordering::Acquire), 0);
    }

    #[test]
    fn results_preserve_native_errors_and_do_not_claim_a_stopped_worker() {
        let action = Action::Unload {
            namespace: "demo".into(),
        };
        let value = json!({"content":[],"structuredContent":{
            "namespace":"demo","status":"stopping","worker_running":true,
            "library_resident_until_process_exit":true
        }});
        let message = result_message(&action, 7, &value).unwrap();
        assert!(message.contains("stop requested"));
        assert!(message.contains("\"worker_running\": true"));
        assert!(message.contains("not forced cancellation or file removal"));

        let error = json!({"isError":true,"content":[{"type":"text","text":"native error\u{1b}[31m\n```"}]});
        let message = result_message(&Action::List, 8, &error).unwrap();
        assert!(message.contains("returned an error"));
        assert!(message.contains("native error\\u001b[31m\\n```"));
        assert!(!message.contains('\u{1b}'));
        assert!(!message.contains("No native plugins"));
        assert!(!message.lines().any(|line| line.starts_with("```")));
    }

    #[test]
    fn empty_listing_and_spilled_result_remain_distinct() {
        let empty = json!({"content":[],"structuredContent":{"plugins":[],"closed":false}});
        assert!(
            result_message(&Action::List, 1, &empty)
                .unwrap()
                .contains("No native plugins are registered")
        );
        let spilled = json!({"content":[],"structuredContent":{"spilled":true,"artifact":{"id":"kept-result"}}});
        let message = result_message(&Action::List, 2, &spilled).unwrap();
        assert!(message.contains("result saved to an output artifact"));
        assert!(message.contains("kept-result"));
        assert!(!message.contains("No native plugins"));
    }
}
