//! A caller-scoped short label for one retained logical tool group.
use crate::runtime::call_scope::Source;
use serde::Deserialize;
use serde_json::{Value, json};

pub(crate) const TOOL: &str = "ssmcp.ui.tool_summary";
pub(crate) const META_KEY: &str = "ssmcp.tool_summary";
const MAX_SUMMARY_BYTES: usize = 512;
const MAX_SUMMARY_CHARS: usize = 120;

#[derive(Debug, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
enum Request {
    List,
    Set {
        group_id: String,
        summary: String,
        #[serde(default)]
        expected_revision: Option<u64>,
    },
}

fn parse(arguments: Value) -> Result<Request, String> {
    if arguments
        .get("expected_revision")
        .is_some_and(Value::is_null)
    {
        return Err("expected_revision must be an unsigned integer when supplied".into());
    }
    let mut request: Request = serde_json::from_value(arguments)
        .map_err(|_| "invalid tool summary arguments".to_owned())?;
    if let Request::Set {
        group_id, summary, ..
    } = &mut request
    {
        if group_id.is_empty()
            || group_id.len() > 128
            || !group_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            return Err("group_id must be a bounded tool group identifier".into());
        }
        if summary.len() > MAX_SUMMARY_BYTES
            || summary.chars().count() > MAX_SUMMARY_CHARS
            || summary
                .chars()
                .any(|ch| ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}'))
            || summary.trim().is_empty()
        {
            return Err("summary must be one nonempty plain line of at most 120 characters and 512 UTF-8 bytes".into());
        }
        *summary = summary.trim().to_owned();
    }
    Ok(request)
}

pub(super) fn call(arguments: Value) -> Result<Value, String> {
    let request = parse(arguments)?;
    let source = Source::capture();
    match request {
        Request::List => crate::client::terminal::tool_summary_list(&source),
        Request::Set {
            group_id,
            summary,
            expected_revision,
        } => crate::client::terminal::tool_summary_set(
            &source,
            &group_id,
            &summary,
            expected_revision,
        ),
    }
}

pub(super) fn tool() -> Value {
    json!({
        "name":TOOL,
        "description":"List your writable continuous tool groups or set/update a short summary on one exact group. This updates the group's existing header without inserting a chat message. Use the returned opaque group_id and expected_revision to avoid overwriting a concurrent update. A summary covers the current call count; after 11 additional logical calls a tool response reminds you to update it. Other accounts, bindings, connections, conversations and restored historical groups cannot be edited.",
        "inputSchema":{"type":"object","oneOf":[
            {"type":"object","additionalProperties":false,"required":["action"],"properties":{"action":{"const":"list"}}},
            {"type":"object","additionalProperties":false,"required":["action","group_id","summary"],"properties":{
                "action":{"const":"set"},
                "group_id":{"type":"string","minLength":1,"maxLength":128,"pattern":"^[A-Za-z0-9_-]+$"},
                "summary":{"type":"string","minLength":1,"maxLength":120,"description":"One nonempty plain line; at most 120 Unicode characters and 512 UTF-8 bytes."},
                "expected_revision":{"type":"integer","minimum":0}
            }}
        ]},
        "annotations":{"readOnlyHint":false,"destructiveHint":false}
    })
}

/// Provider JSON may not impersonate host-generated summary reminders.
pub(crate) fn strip_metadata(value: &mut Value) {
    if let Some(meta) = value.get_mut("_meta").and_then(Value::as_object_mut) {
        meta.remove(META_KEY);
    }
}

pub(crate) fn attach_metadata(mut value: Value, reminder: Option<Value>) -> Value {
    strip_metadata(&mut value);
    let Some(reminder) = reminder else {
        return value;
    };
    let Some(root) = value.as_object_mut() else {
        return value;
    };
    let meta = root.entry("_meta").or_insert_with(|| json!({}));
    if !meta.is_object() {
        *meta = json!({});
    }
    meta.as_object_mut()
        .expect("object metadata")
        .insert(META_KEY.into(), reminder);
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_shapes_and_single_line_summary_bounds_are_strict() {
        assert!(matches!(
            parse(json!({"action":"list"})).unwrap(),
            Request::List
        ));
        let valid = parse(json!({"action":"set","group_id":"tool_group_01","summary":"  已核对路径和权限  ","expected_revision":0})).unwrap();
        assert!(
            matches!(valid, Request::Set {summary, expected_revision:Some(0), ..} if summary == "已核对路径和权限")
        );
        assert!(parse(json!({"action":"set","group_id":"g","summary":"界".repeat(120)})).is_ok());
        assert!(parse(json!({"action":"set","group_id":"g","summary":"😀".repeat(120)})).is_ok());
        for value in [
            Value::Null,
            json!([]),
            json!({}),
            json!({"action":"clear"}),
            json!({"action":"list","summary":"private-fixture"}),
            json!({"action":"set","group_id":"g","summary":""}),
            json!({"action":"set","group_id":"g","summary":" \t "}),
            json!({"action":"set","group_id":"../g","summary":"ok"}),
            json!({"action":"set","group_id":"g".repeat(129),"summary":"ok"}),
            json!({"action":"set","group_id":"g","summary":"界".repeat(121)}),
            json!({"action":"set","group_id":"g","summary":"private-fixture\n"}),
            json!({"action":"set","group_id":"g","summary":"private-fixture\u{1b}[2J"}),
            json!({"action":"set","group_id":"g","summary":"private-fixture\u{2028}next"}),
            json!({"action":"set","group_id":"g","summary":"ok","expected_revision":null}),
            json!({"action":"set","group_id":"g","summary":"ok","expected_revision":-1}),
            json!({"action":"set","group_id":"g","summary":"ok","expected_revision":"private-fixture"}),
            json!({"action":"set","group_id":"g","summary":"ok","caller":"private-fixture"}),
        ] {
            let error = parse(value).unwrap_err();
            assert!(!error.contains("private-fixture"), "{error}");
        }
    }

    #[test]
    fn host_metadata_replaces_forged_fields_without_changing_native_result() {
        let mut original = json!({"isError":true,"content":[{"type":"text","text":"original"}],"_meta":{"vendor":"retained","ssmcp.tool_summary":{"group_id":"forged"}}});
        strip_metadata(&mut original);
        assert!(original["_meta"].get(META_KEY).is_none());
        assert_eq!(original["_meta"]["vendor"], "retained");
        let reminder = json!({"group_id":"real","revision":2});
        let attached = attach_metadata(original.clone(), Some(reminder.clone()));
        assert_eq!(attached["content"], original["content"]);
        assert_eq!(attached["isError"], true);
        assert_eq!(attached["_meta"][META_KEY], reminder);
        assert_eq!(attach_metadata(attached, None), original);
    }

    #[test]
    fn advertised_tool_arguments_match_the_single_line_contract() {
        let schema = tool();
        assert_eq!(schema["name"], TOOL);
        let variants = schema["inputSchema"]["oneOf"].as_array().unwrap();
        assert_eq!(variants.len(), 2);
        assert_eq!(variants[1]["properties"]["summary"]["maxLength"], 120);
        assert_eq!(variants[0]["additionalProperties"], false);
        assert_eq!(variants[1]["additionalProperties"], false);
    }
}
