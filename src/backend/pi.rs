//! Pi's native JSONL control protocol, exposed without reducing it to a prompt wrapper.
//!
//! Protocol reference (Pi 1.5.2):
//! <https://github.com/earendil-works/pi/blob/main/packages/coding-agent/docs/rpc-commands.md>
//! The generic `pi.rpc` tool also forwards commands added by later Pi releases.

use std::{ffi::OsStr, ffi::OsString, sync::Arc};

use serde_json::{Value, json};
use tokio::process::Command;

use super::{
    result,
    rpc::{Dialect, Peer},
};

pub struct Pi {
    peer: Arc<Peer>,
}

impl Pi {
    pub async fn start(program: &OsStr, args: &[OsString]) -> crate::Result<Arc<Self>> {
        let mut command = Command::new(program);
        command.args(["--mode", "rpc"]).args(args);
        let peer = Peer::spawn(command, "pi", Dialect::Pi).await?;
        let ready = peer.request("get_state", json!({})).await;
        let error = match ready {
            Ok(response) if response["success"] == true => None,
            Ok(response) => Some(format!(
                "Pi rejected the startup get_state request: {}",
                response["error"].as_str().unwrap_or("invalid response")
            )),
            Err(error) => Some(format!("Pi RPC startup failed: {error}")),
        };
        if let Some(error) = error {
            peer.shutdown().await;
            return Err(error.into());
        }
        Ok(Arc::new(Self { peer }))
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        Ok(tool_catalog())
    }

    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match name {
            "ssmcp.pi.events" => Ok(result(self.peer.events().read(arguments).await?)),
            "ssmcp.pi.requests" => Ok(result(self.peer.pending().await)),
            "ssmcp.pi.respond" => Ok(result(self.peer.respond(arguments).await?)),
            _ => {
                let (command, params) = request_for(name, arguments)?;
                let response = self.peer.request(&command, params).await?;
                let failed = response.get("success").and_then(Value::as_bool) == Some(false);
                let mut output = result(response);
                // Preserve the complete native error response for callers, while letting
                // ordinary MCP clients distinguish a rejected command from success.
                if failed {
                    output["isError"] = Value::Bool(true);
                }
                Ok(output)
            }
        }
    }

    pub async fn shutdown(&self) {
        self.peer.shutdown().await;
    }
}

/// Name, description, and whether the native command only reads session state.
/// Keep the names independent of the schemas so an unknown command can still be
/// sent explicitly through `pi.rpc` when the installed CLI is newer than ssmcp.
const COMMANDS: &[(&str, &str, bool)] = &[
    (
        "prompt",
        "Send a prompt, image, extension slash command, skill, or prompt template to the local Pi session. Success means accepted/queued/handled, not completed. Consume ssmcp.pi.events; agent_settled means no automatic work remains. A handled slash command may start no run. While running, provide streamingBehavior: steer or followUp.",
        false,
    ),
    (
        "steer",
        "Queue a steering message for the next assistant turn. Expands skills/templates; invoke extension slash commands through pi.prompt instead.",
        false,
    ),
    (
        "follow_up",
        "Queue a message to run after the current agent work finishes. Expands skills/templates; invoke extension slash commands through pi.prompt instead.",
        false,
    ),
    (
        "abort",
        "Abort the current Pi operation and wait for idle. Queued messages may continue; call pi.clear_queue before this command when stopping all queued work.",
        false,
    ),
    (
        "clear_queue",
        "Remove pending steering and follow-up messages and return their text.",
        false,
    ),
    (
        "new_session",
        "Start a new Pi session. An extension can veto the change: inspect data.cancelled even when success is true.",
        false,
    ),
    (
        "get_state",
        "Read the current Pi model, session, streaming, compaction, and queue state.",
        true,
    ),
    (
        "set_model",
        "Select an available model by provider and modelId.",
        false,
    ),
    (
        "cycle_model",
        "Cycle to the next configured model; returns the model and thinking level or null.",
        false,
    ),
    (
        "get_available_models",
        "List models available to the local Pi configuration.",
        true,
    ),
    (
        "set_thinking_level",
        "Set the Pi reasoning level; the installed model may clamp it to its capabilities.",
        false,
    ),
    (
        "cycle_thinking_level",
        "Cycle to the next supported reasoning level.",
        false,
    ),
    (
        "get_available_thinking_levels",
        "List reasoning levels supported by the current model.",
        true,
    ),
    (
        "set_steering_mode",
        "Choose whether steering messages are consumed together or one at a time.",
        false,
    ),
    (
        "set_follow_up_mode",
        "Choose whether follow-up messages are consumed together or one at a time.",
        false,
    ),
    (
        "compact",
        "Compact the current conversation, optionally with custom instructions. This can make model requests.",
        false,
    ),
    (
        "set_auto_compaction",
        "Enable or disable automatic context compaction.",
        false,
    ),
    (
        "set_auto_retry",
        "Enable or disable automatic provider-error retries.",
        false,
    ),
    ("abort_retry", "Cancel a pending automatic retry.", false),
    (
        "bash",
        "Execute a local shell command through Pi. Output streams as bash_execution_update events with the request ID; the final response can be truncated. Output enters conversation context unless excludeFromContext is true.",
        false,
    ),
    (
        "abort_bash",
        "Abort the currently running direct Pi bash command.",
        false,
    ),
    (
        "get_session_stats",
        "Read conversation, token, and cost statistics for the current session.",
        true,
    ),
    (
        "export_html",
        "Export the current conversation to a local HTML file and return its path.",
        false,
    ),
    (
        "switch_session",
        "Open an existing local session file. Inspect data.cancelled for extension vetoes.",
        false,
    ),
    (
        "fork",
        "Fork the session from a user message entry. Returns its text and whether an extension canceled the operation.",
        false,
    ),
    (
        "clone",
        "Clone the current session history into a new session. Inspect data.cancelled for extension vetoes.",
        false,
    ),
    (
        "get_fork_messages",
        "List user-message entry IDs and text available for session forking.",
        true,
    ),
    (
        "get_entries",
        "Read raw session entries, optionally after a previously observed entry ID.",
        true,
    ),
    (
        "get_tree",
        "Read the complete session tree and active leaf ID.",
        true,
    ),
    (
        "get_last_assistant_text",
        "Read the last completed assistant text, or null when absent.",
        true,
    ),
    (
        "set_session_name",
        "Set the human-readable name of the current session.",
        false,
    ),
    (
        "get_messages",
        "Read the current conversation messages with their original content blocks.",
        true,
    ),
    (
        "get_commands",
        "Discover the installed extension slash commands, prompt templates, and skills, including source metadata. Invoke them as /name through pi.prompt. This is not a model-tool catalog; terminal-only built-in slash commands are not listed.",
        true,
    ),
];

fn native_schema(command: &str) -> Value {
    let (properties, required): (Value, &[&str]) = match command {
        "prompt" => (
            json!({
                "message": {"type":"string"},
                "images": image_schema(),
                "streamingBehavior": {"type":"string","enum":["steer","followUp"]}
            }),
            &["message"],
        ),
        "steer" | "follow_up" => (
            json!({"message":{"type":"string"},"images":image_schema()}),
            &["message"],
        ),
        "new_session" => (
            json!({"parentSession":{"type":"string","description":"Optional parent session file path."}}),
            &[],
        ),
        "set_model" => (
            json!({"provider":{"type":"string"},"modelId":{"type":"string"}}),
            &["provider", "modelId"],
        ),
        "set_thinking_level" => (
            json!({"level":{"type":"string","enum":["off","minimal","low","medium","high","xhigh","max"]}}),
            &["level"],
        ),
        "set_steering_mode" | "set_follow_up_mode" => (
            json!({"mode":{"type":"string","enum":["all","one-at-a-time"]}}),
            &["mode"],
        ),
        "compact" => (json!({"customInstructions":{"type":"string"}}), &[]),
        "set_auto_compaction" | "set_auto_retry" => {
            (json!({"enabled":{"type":"boolean"}}), &["enabled"])
        }
        "bash" => (
            json!({
                "command":{"type":"string"},
                "excludeFromContext":{"type":"boolean","default":false}
            }),
            &["command"],
        ),
        "export_html" => (json!({"outputPath":{"type":"string"}}), &[]),
        "switch_session" => (json!({"sessionPath":{"type":"string"}}), &["sessionPath"]),
        "fork" => (json!({"entryId":{"type":"string"}}), &["entryId"]),
        "get_entries" => (
            json!({"since":{"type":"string","description":"Return entries after this entry ID."}}),
            &[],
        ),
        "set_session_name" => (json!({"name":{"type":"string"}}), &["name"]),
        _ => (json!({}), &[]),
    };
    json!({
        "type":"object",
        "properties":properties,
        "required":required,
        "additionalProperties":true
    })
}

fn image_schema() -> Value {
    json!({
        "type":"array",
        "items":{
            "type":"object",
            "properties":{
                "type":{"type":"string","const":"image"},
                "data":{"type":"string","description":"Base64-encoded image data."},
                "mimeType":{"type":"string"}
            },
            "required":["type","data","mimeType"],
            "additionalProperties":true
        }
    })
}

fn tool_catalog() -> Vec<Value> {
    let mut tools: Vec<Value> = COMMANDS
        .iter()
        .map(|(command, description, read_only)| {
            json!({
                "name":format!("pi.{command}"),
                "description":description,
                "inputSchema":native_schema(command),
                "annotations":{
                    "readOnlyHint":read_only,
                    "destructiveHint":!read_only,
                    "openWorldHint":!read_only
                }
            })
        })
        .collect();
    tools.extend([
        json!({
            "name":"pi.rpc",
            "description":"Send any command supported by the installed Pi JSONL protocol, including commands introduced after ssmcp. type is the native command name; args contains its fields. ssmcp assigns correlation IDs. For extension dialog replies use ssmcp.pi.respond. Preserve the native response and inspect success; prompt acceptance does not mean agent completion.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "type":{"type":"string","minLength":1},
                    "args":{"type":"object","additionalProperties":true,"default":{}}
                },
                "required":["type"],
                "additionalProperties":false
            },
            "annotations":{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true}
        }),
        json!({
            "name":"ssmcp.pi.events",
            "description":"Read Pi events and diagnostics from a bounded cursor log. Reuse next_cursor; truncated means older records were lost. wait_ms waits for new records, up to 5 seconds. Read agent_settled for final idle; agent_end alone may be followed by retries, compaction, or queued work. Local terminal logs stream independently.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "cursor":{"type":"integer","minimum":0,"default":0},
                    "max_events":{"type":"integer","minimum":1,"maximum":256,"default":50},
                    "wait_ms":{"type":"integer","minimum":0,"maximum":5000,"default":0}
                },
                "additionalProperties":false
            },
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        }),
        json!({
            "name":"ssmcp.pi.requests",
            "description":"List pending Pi extension dialogs (select, confirm, input, editor). Dialogs are never approved or filled automatically. Pi can time out a dialog independently, so a listed request may have expired. Informational UI events are available through ssmcp.pi.events.",
            "inputSchema":{"type":"object","additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}
        }),
        json!({
            "name":"ssmcp.pi.respond",
            "description":"Explicitly answer one pending Pi extension dialog with its original id. For select/input/editor provide value; for confirm provide confirmed; or dismiss with cancelled:true. Exactly one answer field is accepted. No approval is inferred. A native extension_ui_response has no separate Pi response; success here means it was sent. Duplicate and unknown IDs are rejected.",
            "inputSchema":{
                "type":"object",
                "properties":{
                    "id":{"type":"string"},
                    "value":{"type":"string"},
                    "confirmed":{"type":"boolean"},
                    "cancelled":{"type":"boolean","const":true}
                },
                "required":["id"],
                "oneOf":[{"required":["value"]},{"required":["confirmed"]},{"required":["cancelled"]}],
                "additionalProperties":false
            },
            "annotations":{"readOnlyHint":false,"destructiveHint":true,"openWorldHint":true}
        }),
    ]);
    tools
}

fn request_for(name: &str, arguments: Value) -> Result<(String, Value), String> {
    let (command, params) = if name == "pi.rpc" {
        let mut object = arguments
            .as_object()
            .cloned()
            .ok_or("pi.rpc arguments must be an object")?;
        let command = object
            .remove("type")
            .and_then(|value| value.as_str().map(str::to_owned))
            .filter(|value| !value.is_empty())
            .ok_or("pi.rpc requires a nonempty string type")?;
        let params = object.remove("args").unwrap_or_else(|| json!({}));
        if !object.is_empty() {
            return Err("pi.rpc command fields belong inside args".into());
        }
        (command, params)
    } else {
        let command = name
            .strip_prefix("pi.")
            .filter(|command| COMMANDS.iter().any(|(known, _, _)| known == command))
            .ok_or_else(|| format!("Unknown Pi tool: {name}"))?;
        (command.to_owned(), arguments)
    };
    if command == "extension_ui_response" {
        return Err("Use ssmcp.pi.respond to answer a pending extension dialog".into());
    }
    let object = params
        .as_object()
        .ok_or("Pi command arguments must be an object")?;
    if object.contains_key("id") || object.contains_key("type") {
        return Err("Pi command arguments cannot override the protocol id or type".into());
    }
    Ok((command, params))
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn catalog_covers_native_control_commands_and_keeps_parameters_extensible() {
        let tools = tool_catalog();
        assert_eq!(COMMANDS.len(), 33);
        assert_eq!(tools.len(), 37);
        let names: HashSet<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect();
        assert_eq!(names.len(), tools.len());
        for (command, _, _) in COMMANDS {
            let name = format!("pi.{command}");
            let tool = tools.iter().find(|tool| tool["name"] == name).unwrap();
            assert_eq!(tool["inputSchema"]["type"], "object");
            assert_eq!(tool["inputSchema"]["additionalProperties"], true);
            for required in tool["inputSchema"]["required"].as_array().unwrap() {
                assert!(
                    tool["inputSchema"]["properties"]
                        .get(required.as_str().unwrap())
                        .is_some()
                );
            }
        }
        assert!(names.contains("pi.clone"));
        assert!(names.contains("pi.get_entries"));
        assert!(names.contains("pi.get_tree"));
        assert!(names.contains("pi.get_commands"));
        assert!(names.contains("ssmcp.pi.respond"));
    }

    #[test]
    fn forwards_images_additional_fields_and_future_commands_without_rewriting() {
        let args = json!({
            "message":"inspect this",
            "images":[{"type":"image","data":"aGVsbG8=","mimeType":"image/png"}],
            "streamingBehavior":"followUp",
            "futureOption":{"nested":true}
        });
        assert_eq!(
            request_for("pi.prompt", args.clone()).unwrap(),
            ("prompt".into(), args)
        );
        let params = json!({"entry":"abc","futureOption":[1,2,3]});
        assert_eq!(
            request_for(
                "pi.rpc",
                json!({"type":"future_command","args":params.clone()})
            )
            .unwrap(),
            ("future_command".into(), params)
        );
        assert_eq!(
            request_for("pi.rpc", json!({"type":"get_state"})).unwrap(),
            ("get_state".into(), json!({}))
        );
    }

    #[test]
    fn protocol_fields_and_dialog_responses_cannot_bypass_the_peer() {
        for args in [json!({"id":"spoof"}), json!({"type":"abort"}), json!([])] {
            assert!(request_for("pi.get_state", args).is_err());
        }
        for args in [
            json!({"type":"extension_ui_response","args":{"id":"pending","confirmed":true}}),
            json!({"type":"get_state","args":{"id":"spoof"}}),
            json!({"type":"get_state","args":null}),
            json!({"type":"prompt","message":"outside args"}),
            json!({"type":""}),
            json!({}),
        ] {
            assert!(request_for("pi.rpc", args).is_err());
        }
        assert!(request_for("pi.unknown", json!({})).is_err());
        assert!(request_for("shell.read", json!({})).is_err());
    }
}
