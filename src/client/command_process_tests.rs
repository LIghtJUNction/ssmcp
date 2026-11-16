//! Command RPC IDs and streaming process IDs are independent native identifiers.
use super::*;

fn start(renderer: &mut Renderer, output: &mut Vec<u8>, call: u64, rpc: &str, process: &str) {
    renderer
        .event(
            output,
            "codex",
            &json!({
                "type":"ssmcp.harness.tool.started","provider":"codex",
                "tool":"codex.command_exec","call_id":call,
                "rpc_correlation":true,"command_rpc_expected":true,
            }),
        )
        .unwrap();
    renderer
        .event(
            output,
            "codex",
            &json!({
                "type":"ssmcp.request","harness_call_id":call,
                "request":{"id":rpc,"method":"command/exec","params":{
                    "command":["fixture","--check"],"processId":process,
                }},
            }),
        )
        .unwrap();
}

fn partial_json(renderer: &mut Renderer, output: &mut Vec<u8>, process: &str, phase: &str) {
    renderer
        .event(
            output,
            "codex",
            &json!({
                "method":"command/exec/outputDelta","params":{
                    "processId":process,"stream":"stdout",
                    "deltaBase64":STANDARD.encode(format!("{{\"phase\":\"{phase}\"}}")),
                },
            }),
        )
        .unwrap();
}

fn complete(renderer: &mut Renderer, output: &mut Vec<u8>, call: u64, rpc: &str) {
    renderer
        .event(
            output,
            "codex",
            &json!({
                "id":rpc,"harness_call_id":call,"result":{"exitCode":0},
            }),
        )
        .unwrap();
    renderer
        .event(
            output,
            "codex",
            &json!({
                "type":"ssmcp.harness.tool.completed","provider":"codex",
                "tool":"codex.command_exec","call_id":call,"success":true,
                "rpc_correlation":true,"command_rpc_expected":true,
                "command_rpc_started":true,"command_rpc_completed":true,
            }),
        )
        .unwrap();
}

#[test]
fn command_completion_flushes_the_distinct_process_id_without_a_newline() {
    let mut renderer = Renderer::new(false, false, 120);
    let mut output = Vec::new();
    start(&mut renderer, &mut output, 301, "rpc-301", "build-7");
    partial_json(&mut renderer, &mut output, "build-7", "alpha-finished");
    assert!(renderer.streams.contains_key("codex\0build-7\0stdout"));
    assert!(!String::from_utf8_lossy(&output).contains("alpha-finished"));

    complete(&mut renderer, &mut output, 301, "rpc-301");
    let text = String::from_utf8(output).unwrap();
    let heading = text
        .lines()
        .find(|line| line.starts_with("[STDOUT]"))
        .unwrap();
    assert!(
        heading.contains("rpc-301") && heading.contains("build-7"),
        "{text}"
    );
    assert!(
        text.find("alpha-finished").unwrap() < text.find("[OK  ]").unwrap(),
        "{text}"
    );
    assert!(!renderer.streams.contains_key("codex\0build-7\0stdout"));
    assert!(renderer.requests.is_empty());
    assert!(renderer.deferred_commands.is_empty());
    assert_eq!(renderer.activities.len(), 0);
}

#[test]
fn parallel_process_ids_flush_only_the_command_that_completed() {
    let mut renderer = Renderer::new(false, false, 120);
    let mut output = Vec::new();
    start(&mut renderer, &mut output, 311, "rpc-311", "process-A");
    start(&mut renderer, &mut output, 312, "rpc-312", "process-B");
    partial_json(&mut renderer, &mut output, "process-A", "alpha-finished");
    partial_json(&mut renderer, &mut output, "process-B", "beta-finished");
    assert!(renderer.streams.contains_key("codex\0process-A\0stdout"));
    assert!(renderer.streams.contains_key("codex\0process-B\0stdout"));

    complete(&mut renderer, &mut output, 312, "rpc-312");
    let intermediate = String::from_utf8_lossy(&output);
    assert!(intermediate.contains("beta-finished"), "{intermediate}");
    assert!(!intermediate.contains("alpha-finished"), "{intermediate}");
    assert!(renderer.streams.contains_key("codex\0process-A\0stdout"));
    assert!(!renderer.streams.contains_key("codex\0process-B\0stdout"));
    assert_eq!(renderer.activities.len(), 1);

    complete(&mut renderer, &mut output, 311, "rpc-311");
    let text = String::from_utf8(output).unwrap();
    assert_eq!(text.matches("alpha-finished").count(), 1, "{text}");
    assert_eq!(text.matches("beta-finished").count(), 1, "{text}");
    assert!(text.find("beta-finished").unwrap() < text.find("alpha-finished").unwrap());
    assert!(!renderer.streams.contains_key("codex\0process-A\0stdout"));
    assert!(renderer.requests.is_empty());
    assert!(renderer.deferred_commands.is_empty());
    assert_eq!(renderer.activities.len(), 0);
}

#[test]
fn command_failure_without_a_response_flushes_its_real_process_stream() {
    let mut renderer = Renderer::new(false, false, 120);
    let mut output = Vec::new();
    start(&mut renderer, &mut output, 321, "rpc-321", "check-9");
    partial_json(&mut renderer, &mut output, "check-9", "last-observed-phase");
    renderer
        .event(
            &mut output,
            "codex",
            &json!({
                "type":"ssmcp.harness.tool.completed","provider":"codex",
                "tool":"codex.command_exec","call_id":321,"success":false,
                "rpc_correlation":true,"command_rpc_expected":true,
                "command_rpc_started":true,"command_rpc_completed":false,
            }),
        )
        .unwrap();
    let text = String::from_utf8(output).unwrap();
    assert!(text.contains("last-observed-phase"), "{text}");
    assert!(text.contains("[ERR ]") && text.contains("failed"), "{text}");
    assert!(!renderer.streams.contains_key("codex\0check-9\0stdout"));
    assert!(renderer.requests.is_empty());
    assert!(renderer.deferred_commands.is_empty());
    assert_eq!(renderer.activities.len(), 0);
}
