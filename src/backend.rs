//! Native tools are always available; optional external harnesses load only on request.
pub mod cc;
pub mod codex;
pub mod events;
pub mod opencode;
pub(crate) mod peer;
pub mod pi;
pub mod process;
pub mod rpc;
pub mod shell;

// Keep the established public startup API while the implementation belongs to
// the process capability plugin. Runtime dispatch crosses the zplugin ABI.
pub use crate::runtime::native_plugins::process::Process as Backend;
use crate::wire::Operation;
#[cfg(test)]
use crate::{
    runtime::native_plugins::process::diagnostic_changed,
    wire::{MAX_READ, MAX_WAIT_MS, MAX_WRITE},
};
use serde_json::{Value, json};
use std::{process::Stdio, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Run a bounded, literal helper for local facilities such as clipboard and notifications.
/// This is not a registered computer action or an embedded desktop backend.
pub(crate) async fn run_helper(
    root: &std::path::Path,
    program: &str,
    args: &[String],
    input: Option<&[u8]>,
    limit: usize,
    deadline: Duration,
) -> Result<Vec<u8>, String> {
    let executable = crate::program::executable(program)?;
    let mut command = tokio::process::Command::new(executable);
    command
        .args(args)
        .current_dir(root)
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut process = process::Managed::spawn(&mut command).map_err(|e| format!("{program}: {e}"))?;
    let mut stdout = process
        .child
        .stdout
        .take()
        .ok_or("helper stdout unavailable")?
        .take((limit + 1) as u64);
    let mut stderr = process
        .child
        .stderr
        .take()
        .ok_or("helper stderr unavailable")?
        .take(8193);
    let mut stdin = process.child.stdin.take();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let outcome = tokio::time::timeout(deadline, async {
        let writer = async {
            if let (Some(mut stdin), Some(input)) = (stdin.take(), input) {
                stdin.write_all(input).await.map_err(|e| e.to_string())?;
                stdin.shutdown().await.map_err(|e| e.to_string())?;
            }
            Ok::<(), String>(())
        };
        let (a, b, c, status) = tokio::join!(
            stdout.read_to_end(&mut out),
            stderr.read_to_end(&mut err),
            writer,
            process.wait()
        );
        a.map_err(|e| e.to_string())?;
        b.map_err(|e| e.to_string())?;
        c?;
        let status = status.map_err(|e| e.to_string())?;
        if out.len() > limit || err.len() > 8192 {
            return Err(format!("{program} exceeded its bounded output limit"));
        }
        if !status.success() {
            return Err(format!(
                "{program} failed ({status}): {}",
                String::from_utf8_lossy(&err)
                    .trim()
                    .chars()
                    .take(1024)
                    .collect::<String>()
            ));
        }
        Ok(out)
    })
    .await
    .map_err(|_| format!("{program} timed out after {} seconds", deadline.as_secs()))
    .and_then(|r| r);
    process.shutdown().await;
    outcome
}

/// Convert a shell capability into a validated private wire operation.
pub(crate) fn terminal_operation(name: &str, arguments: Value) -> Result<Operation, String> {
    let op = match name {
        "terminal.read" => "read",
        "terminal.write" => "write",
        "terminal.resize" => "resize",
        "terminal.status" => "status",
        _ => return Err(format!("unknown shell capability: {name}")),
    };
    let mut arguments = arguments
        .as_object()
        .cloned()
        .ok_or("tool arguments must be an object")?;
    if arguments.contains_key("op") {
        return Err("unexpected tool argument: op".into());
    }
    arguments.insert("op".into(), Value::String(op.into()));
    let operation: Operation = serde_json::from_value(Value::Object(arguments))
        .map_err(|e| format!("invalid shell arguments: {e}"))?;
    operation.validate()?;
    Ok(operation)
}

// The cursor binds to catalog names/schema content, so list changes cannot silently skip tools.
#[cfg(test)]
pub(crate) fn page(tools: impl AsRef<[Value]>, cursor: Option<&str>) -> Result<Value, String> {
    let tools = tools.as_ref();
    let encoded = serde_json::to_vec(tools).map_err(|e| e.to_string())?;
    // A non-cryptographic fingerprint is sufficient: this is stale-page detection, not auth.
    let hash = encoded.iter().fold(0xcbf29ce484222325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x100000001b3)
    });
    page_with_fingerprint(tools, cursor, &format!("{hash:016x}"))
}

#[cfg(test)]
pub(crate) fn page_with_fingerprint(
    tools: &[Value],
    cursor: Option<&str>,
    fingerprint: &str,
) -> Result<Value, String> {
    let start = match cursor {
        None => 0,
        Some(cursor) => {
            let (requested, index) = cursor.split_once(':').ok_or("invalid tool cursor")?;
            if requested != fingerprint {
                return Err("tool catalog changed; restart tools/list without a cursor".into());
            }
            index.parse::<usize>().map_err(|_| "invalid tool cursor")?
        }
    };
    if start > tools.len() {
        return Err("tool cursor is beyond the catalog".into());
    }
    let mut end = start;
    let mut bytes = 0;
    while end < tools.len() && end - start < 64 {
        let length = tools[end].to_string().len();
        if length > 896 * 1024 {
            return Err("one tool schema exceeds the transport limit".into());
        }
        if end > start && bytes + length > 512 * 1024 {
            break;
        }
        bytes += length;
        end += 1;
    }
    let mut page = json!({"tools":&tools[start..end]});
    if end < tools.len() {
        page["nextCursor"] = json!(format!("{fingerprint}:{end}"));
    }
    Ok(page)
}

pub fn result(value: Value) -> Value {
    let mut output = json!({"content":[{"type":"text","text":value.to_string()}],"isError":false});
    if value.is_object() {
        output["structuredContent"] = value;
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[tokio::test]
    async fn helpers_preserve_exit_errors_and_deadlines() {
        let args = vec![
            "-c".into(),
            "import sys;sys.stderr.write('native failure');sys.exit(7)".into(),
        ];
        let err = run_helper(
            &std::env::temp_dir(),
            "python3",
            &args,
            None,
            1024,
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(err.contains("native failure"));
        let args = vec!["-c".into(), "import time;time.sleep(10)".into()];
        let err = run_helper(
            &std::env::temp_dir(),
            "python3",
            &args,
            None,
            1024,
            Duration::from_millis(30),
        )
        .await
        .unwrap_err();
        assert!(err.contains("timed out"));
    }
    #[cfg(unix)]
    #[tokio::test]
    async fn helper_input_is_literal_and_output_limit_is_enforced() {
        let args = vec![
            "-c".into(),
            "import sys;sys.stdout.buffer.write(sys.stdin.buffer.read())".into(),
        ];
        let input = b"$(not-a-command) ' \n --file=/etc/passwd";
        assert_eq!(
            run_helper(
                &std::env::temp_dir(),
                "python3",
                &args,
                Some(input),
                1024,
                Duration::from_secs(2)
            )
            .await
            .unwrap(),
            input
        );
        let err = run_helper(
            &std::env::temp_dir(),
            "python3",
            &args,
            Some(&[b'a'; 100]),
            16,
            Duration::from_secs(2),
        )
        .await
        .unwrap_err();
        assert!(err.contains("bounded output"));
    }

    #[test]
    fn discovery_attempt_metadata_does_not_repeat_a_stable_provider_status() {
        let previous = json!({"provider":"codex","status":"ready","tools":1342,
            "details":{"generation":1,"refreshing":false,"status":"ready","reason":null}});
        let mut polled = previous.clone();
        polled["details"]["generation"] = 99.into();
        polled["details"]["refreshing"] = true.into();
        assert!(!diagnostic_changed(&previous, &polled));
        let mut changed = polled.clone();
        changed["tools"] = 1341.into();
        assert!(diagnostic_changed(&previous, &changed));
        changed = polled;
        changed["details"]["status"] = "unavailable".into();
        changed["details"]["reason"] = "native discovery failed".into();
        assert!(diagnostic_changed(&previous, &changed));
    }

    #[test]
    fn shell_capabilities_use_the_same_validated_dispatch_inside_code_mode() {
        assert!(matches!(
            terminal_operation("terminal.read", json!({})),
            Ok(Operation::Read {
                cursor: 0,
                max_bytes: 16384,
                wait_ms: 0
            })
        ));
        for arguments in [
            json!({"max_bytes":MAX_READ+1}),
            json!({"wait_ms":MAX_WAIT_MS+1}),
            json!({"cursor":-1}),
            json!({"op":"status"}),
            json!({"extra":true}),
        ] {
            assert!(terminal_operation("terminal.read", arguments).is_err());
        }
        assert!(
            terminal_operation("terminal.write", json!({"data":"界".repeat(MAX_WRITE/3+1)}))
                .is_err()
        );
        assert!(terminal_operation("terminal.resize", json!({"rows":0,"cols":80})).is_err());
        assert!(terminal_operation("unknown", json!({})).is_err());
    }

    #[test]
    fn tool_pagination_detects_catalog_changes() {
        let tools: Vec<_> = (0..150)
            .map(|n| json!({"name":format!("tool{n}")}))
            .collect();
        let first = page(tools.clone(), None).unwrap();
        assert_eq!(first["tools"].as_array().unwrap().len(), 64);
        let second = page(tools.clone(), first["nextCursor"].as_str()).unwrap();
        assert_eq!(second["tools"][0]["name"], "tool64");
        let third = page(tools.clone(), second["nextCursor"].as_str()).unwrap();
        assert_eq!(third["tools"].as_array().unwrap().len(), 22);
        assert!(third.get("nextCursor").is_none());
        assert!(page(&tools[1..], first["nextCursor"].as_str()).is_err());
    }
}
