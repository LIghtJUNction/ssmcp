//! Explicit native execution under the existing exclusive local input lease.
use super::*;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Start {
    command: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cancel {
    job_id: String,
}

pub(super) fn commands() -> Vec<Command> {
    vec![
        Command { id:id("exec.start"),description:"Start one argv command in this backend's native execution registry under the private input lease; native hooks and policy apply; never replay a missing receipt".into(),
            input_schema:json!({"type":"object","additionalProperties":false,"properties":{
                "command":{"type":"array","minItems":1,"maxItems":128,"items":{"type":"string","maxLength":8192}}},"required":["command"]}) },
        Command { id:id("exec.cancel"),description:"Cancel one exact existing native job under the private input lease and return its actual receipt".into(),
            input_schema:json!({"type":"object","additionalProperties":false,"properties":{
                "job_id":{"type":"string","minLength":1,"maxLength":128}},"required":["job_id"]}) },
    ]
}

fn operation(command: &str, arguments: Value) -> Result<crate::wire::Operation, BridgeError> {
    let arguments = match command {
        "exec.start" => {
            let args: Start = serde_json::from_value(arguments)
                .map_err(|_| BridgeError::invalid("invalid execution arguments"))?;
            if args.command.is_empty()
                || args.command.len() > 128
                || args.command[0].is_empty()
                || args
                    .command
                    .iter()
                    .any(|value| value.len() > 8192 || value.contains('\0'))
                || args.command.iter().map(String::len).sum::<usize>() > 24 * 1024
            {
                return Err(BridgeError::invalid("execution argv exceeds its bounds"));
            }
            json!({"command":args.command})
        }
        "exec.cancel" => {
            let args: Cancel = serde_json::from_value(arguments)
                .map_err(|_| BridgeError::invalid("invalid cancellation arguments"))?;
            if args.job_id.is_empty()
                || args.job_id.len() > 128
                || args.job_id.chars().any(char::is_control)
            {
                return Err(BridgeError::invalid("invalid native job identifier"));
            }
            // The implicit main terminal belongs to the connection's lifecycle.
            if args.job_id == "main" {
                return Err(BridgeError::new(
                    ErrorCode::Denied,
                    "cancel a submitted job, not the connection's main terminal",
                ));
            }
            json!({"job_id":args.job_id})
        }
        _ => {
            return Err(BridgeError::new(
                ErrorCode::Denied,
                "unknown execution command",
            ));
        }
    };
    Ok(crate::wire::Operation::CallTool {
        name: format!("ssmcp.{command}"),
        arguments,
    })
}

pub(super) async fn perform(
    host: &BoundHost<'_>,
    runtime: &Arc<Runtime>,
    command: &str,
    arguments: Value,
) -> Result<Value, BridgeError> {
    let operation = operation(command, arguments)?;
    host.authorize(Access::Control)?;
    host.runtime()?;
    // Runtime::perform includes registry generation, native preflight, hooks,
    // audit, process ownership and local history. No direct shell bypass.
    runtime
        .perform(operation)
        .await
        .map_err(|error| native(error).uncertain())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_execution_preserves_argv_and_rejects_extra_authority() {
        let value = operation("exec.start", json!({"command":["printf","%s","a b"]})).unwrap();
        let crate::wire::Operation::CallTool { name, arguments } = value else {
            panic!("expected exact native operation")
        };
        assert_eq!(name, "ssmcp.exec.start");
        assert_eq!(arguments["command"][2], "a b");
        for value in [
            json!({"command":[]}),
            json!({"command":[""]}),
            json!({"command":["echo"],"agent_id":"forged"}),
            json!({"command":["echo"],"cwd":"/"}),
            json!({"command":["echo","x".repeat(8193)]}),
        ] {
            assert!(operation("exec.start", value).is_err());
        }
        assert!(operation("exec.cancel", json!({"job_id":"main"})).is_err());
        assert!(operation("exec.cancel", json!({"job_id":"a","force":true})).is_err());
        assert!(operation("exec.cancel", json!({"job_id":"job-123"})).is_ok());
    }
}
