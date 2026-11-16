//! Replays command events through the production renderer; no commands execute.
//! Set SSMCP_READABLE_LOG_PREVIEW_DIR to export the 80/100-column ANSI samples.
use super::*;

fn wrapper_started(call_id: u64) -> Value {
    json!({"type":"ssmcp.harness.tool.started","call_id":call_id,
        "provider":"codex","tool":"codex.command_exec","rpc_correlation":true,
        "command_rpc_expected":true})
}

fn wrapper_completed(call_id: u64) -> Value {
    json!({"type":"ssmcp.harness.tool.completed","call_id":call_id,
        "provider":"codex","tool":"codex.command_exec","rpc_correlation":true,
        "command_rpc_expected":true,"command_rpc_started":true,
        "command_rpc_completed":true,"success":true})
}

fn command_events() -> Vec<Value> {
    let read_id = "1234567890abcdef1234567890abcdef-119";
    let edit_id = "1234567890abcdef1234567890abcdef-120";
    let check_id = "1234567890abcdef1234567890abcdef-121";
    let read_script = concat!(
        "from pathlib import Path\n",
        "import tomllib\n",
        "source = Path('Cargo.toml')\n",
        "package = tomllib.loads(source.read_text())['package']\n",
        "print(f\"Package: {package['name']}\")\n",
        "print(f\"Version: {package['version']}\")\n",
        "print('配置读取完成')\n",
    );
    let edit_script = include_str!("../../tests/fixtures/readable-command.py");
    assert_eq!(edit_script.lines().count(), 83);
    vec![
        wrapper_started(110),
        json!({"type":"ssmcp.request","harness_call_id":110,"request":{
            "id":read_id,"method":"command/exec","params":{
                "command":["python3","-c",read_script],
                "cwd":"/workspace/ssmcp","timeoutMs":18000
            }
        }}),
        wrapper_started(111),
        json!({"type":"ssmcp.request","harness_call_id":111,"request":{
            "id":edit_id,"method":"command/exec","params":{
                "command":["python3","-c",edit_script],
                "cwd":"/workspace/ssmcp/generated/magicnet-android","timeoutMs":15000
            }
        }}),
        json!({"method":"command/exec/outputDelta","params":{
            "processId":edit_id,"stream":"stdout",
            "deltaBase64":STANDARD.encode("Updated manifest.json\n")
        }}),
        json!({"harness_call_id":111,"id":edit_id,"result":{"exitCode":0}}),
        wrapper_completed(111),
        json!({"method":"command/exec/outputDelta","params":{
            "processId":read_id,"stream":"stdout",
            "deltaBase64":STANDARD.encode("Package: ssmcp\nVersion: 0.6.1\n配置读取完成\n")
        }}),
        json!({"harness_call_id":110,"id":read_id,"result":{"exitCode":0}}),
        wrapper_completed(110),
        wrapper_started(112),
        json!({"type":"ssmcp.request","harness_call_id":112,"request":{
            "id":check_id,"method":"command/exec","params":{
                "command":["cargo","check","--locked"],
                "cwd":"/workspace/ssmcp","timeoutMs":18000
            }
        }}),
        json!({"method":"command/exec/outputDelta","params":{
            "processId":check_id,"stream":"stderr",
            "deltaBase64":STANDARD.encode("error: fixture compile failure at src/main.rs:17\n")
        }}),
        json!({"harness_call_id":112,"id":check_id,"result":{"exitCode":101}}),
        wrapper_completed(112),
    ]
}

#[test]
fn readable_inline_command_fixture_at_80_and_100_columns() {
    for width in [80, 100] {
        let mut renderer = Renderer::new(false, true, width);
        renderer.ui.theme = super::super::ui::Theme::Minimal;
        renderer.workspace_root = Some("/workspace/ssmcp".into());
        let mut output = Vec::new();
        for event in command_events() {
            renderer.event(&mut output, "codex", &event).unwrap();
        }
        renderer.finish_group(&mut output, "codex").unwrap();
        let plain = clean(std::str::from_utf8(&output).unwrap());
        for visible in [
            "python3",
            "Cargo.toml",
            "source.read_text()",
            "配置读取完成",
            "target.write_text",
            "generated/magicnet-android",
            "Updated manifest.json",
            "cargo check --locked",
            "fixture compile failure",
            "101",
            "#119",
            "#120",
            "#121",
        ] {
            assert!(
                plain.contains(visible),
                "{width} columns: missing {visible:?}\n{plain}"
            );
        }
        assert!(
            plain.contains("folded"),
            "{width} columns: missing folding explanation\n{plain}"
        );
        assert_eq!(plain.matches("[ERR ] codex").count(), 1, "{plain}");
        for duplicate in ["codex.command_exec", "call=110", "call=111", "call=112"] {
            assert!(
                !plain.contains(duplicate),
                "duplicate wrapper: {duplicate}\n{plain}"
            );
        }
        for line in plain.lines() {
            assert!(
                unicode_width::UnicodeWidthStr::width(line) <= width,
                "{width} columns: transcript overflow\n{line}"
            );
        }
        if let Some(directory) = std::env::var_os("SSMCP_READABLE_LOG_PREVIEW_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            std::fs::write(
                directory.join(format!("readable-logs-{width}.ansi")),
                &output,
            )
            .unwrap();
            std::fs::write(directory.join(format!("readable-logs-{width}.txt")), &plain).unwrap();
        }
    }
}

#[test]
fn dispatch_failure_keeps_error_and_removes_pending_activity() {
    // Exercise failure both before a command request and after dispatch without
    // a native response. Neither case may silently disappear or stay "running".
    for request_started in [false, true] {
        let mut renderer = Renderer::new(false, false, 100);
        let mut output = Vec::new();
        renderer
            .event(&mut output, "codex", &wrapper_started(210))
            .unwrap();
        assert_eq!(renderer.activities.len(), 1);
        if request_started {
            renderer
                .event(
                    &mut output,
                    "codex",
                    &json!({
                        "type":"ssmcp.request","harness_call_id":210,
                        "request":{"id":"dispatch-210","method":"command/exec",
                            "params":{"command":["cargo","check"]}}
                    }),
                )
                .unwrap();
        }
        let mut failed = wrapper_completed(210);
        failed["success"] = false.into();
        failed["command_rpc_started"] = request_started.into();
        failed["command_rpc_completed"] = false.into();
        renderer.event(&mut output, "codex", &failed).unwrap();
        let plain = String::from_utf8(output).unwrap();
        assert!(
            plain.contains("[ERR ") && plain.contains("failed"),
            "{plain}"
        );
        assert!(plain.contains("call=210"), "{plain}");
        assert_eq!(renderer.activities.len(), 0);
        assert!(renderer.deferred_commands.is_empty());
    }
}

#[test]
fn command_response_without_exit_code_is_unknown_not_success() {
    let mut renderer = Renderer::new(false, false, 100);
    let mut output = Vec::new();
    for event in [
        wrapper_started(211),
        json!({"type":"ssmcp.request","harness_call_id":211,
            "request":{"id":"unknown-211","method":"command/exec",
                "params":{"command":["cargo","check"]}}}),
        json!({"id":"unknown-211","harness_call_id":211,"result":{}}),
        wrapper_completed(211),
    ] {
        renderer.event(&mut output, "codex", &event).unwrap();
    }
    let plain = String::from_utf8(output).unwrap();
    assert!(
        plain.contains("[WARN]") && plain.contains("exit unknown"),
        "{plain}"
    );
    assert!(!plain.contains("[OK  ]"), "{plain}");
    assert_eq!(renderer.activities.len(), 0);
    assert!(renderer.deferred_commands.is_empty());
    assert!(renderer.requests.is_empty());
}

#[test]
fn verbose_and_jsonl_keep_wrapper_audit_metadata() {
    for raw in [false, true] {
        let mut renderer = Renderer::new(raw, false, 100);
        renderer.verbose = !raw;
        let mut output = Vec::new();
        for event in command_events() {
            renderer.event(&mut output, "codex", &event).unwrap();
        }
        let text = String::from_utf8(output).unwrap();
        for retained in [
            "ssmcp.harness.tool.started",
            "ssmcp.harness.tool.completed",
            "harness_call_id",
            "command_rpc_completed",
            "command_rpc_expected",
        ] {
            assert!(
                text.contains(retained),
                "raw={raw}: missing {retained}\n{text}"
            );
        }
        if raw {
            let events = text
                .lines()
                .map(|line| serde_json::from_str::<Value>(line).unwrap())
                .collect::<Vec<_>>();
            assert_eq!(events.len(), command_events().len());
            assert_eq!(
                events
                    .iter()
                    .filter(|event| event["type"] == "ssmcp.harness.tool.started")
                    .count(),
                3
            );
        }
    }
}

#[test]
fn identical_parallel_scripts_finish_by_id_without_activity_leaks() {
    let mut renderer = Renderer::new(false, false, 100);
    let mut output = Vec::new();
    let command = json!([
        "python3",
        "-c",
        "from pathlib import Path\nprint(Path('Cargo.toml').read_text())"
    ]);
    for call_id in [220, 221] {
        renderer
            .event(&mut output, "codex", &wrapper_started(call_id))
            .unwrap();
        renderer
            .event(
                &mut output,
                "codex",
                &json!({
                    "type":"ssmcp.request","harness_call_id":call_id,
                    "request":{"id":format!("parallel-{call_id}"),"method":"command/exec",
                        "params":{"command":command}}
                }),
            )
            .unwrap();
    }
    let tasks = renderer.activities.snapshot();
    assert_eq!(tasks.len(), 2);
    assert!(tasks.iter().any(|task| task.id == "codex:parallel-220"));
    assert!(tasks.iter().any(|task| task.id == "codex:parallel-221"));
    for (call_id, exit_code) in [(221, 9), (220, 0)] {
        renderer
            .event(
                &mut output,
                "codex",
                &json!({
                    "harness_call_id":call_id,"id":format!("parallel-{call_id}"),
                    "result":{"exitCode":exit_code}
                }),
            )
            .unwrap();
        renderer
            .event(&mut output, "codex", &wrapper_completed(call_id))
            .unwrap();
        if call_id == 221 {
            let remaining = renderer.activities.snapshot();
            assert_eq!(remaining.len(), 1);
            assert_eq!(remaining[0].id, "codex:parallel-220");
        }
    }
    let plain = String::from_utf8(output).unwrap();
    assert!(
        plain
            .lines()
            .any(|line| line.starts_with("[ERR ]") && line.contains("parallel-221")),
        "{plain}"
    );
    assert!(
        plain
            .lines()
            .any(|line| line.starts_with("[OK  ]") && line.contains("parallel-220")),
        "{plain}"
    );
    assert!(!plain.contains("codex.command_exec"), "{plain}");
    assert_eq!(renderer.activities.len(), 0);
    assert!(renderer.deferred_commands.is_empty());
    assert!(renderer.requests.is_empty());
}
