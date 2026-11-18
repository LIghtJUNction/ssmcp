//! Redacted audit copies. Native tool responses and stored output bytes are unchanged.
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};

const REDACTED: &str = "[redacted]";
const DEPTH_LIMIT: usize = 24;

/// Sanitize a copy before rendering or retaining an audit event. Apply this at
/// audit boundaries only, never to the value returned by a native tool.
pub fn event(value: Value) -> Value {
    redact(value, false, 0)
}

fn redact(value: Value, inherited_sensitive: bool, depth: usize) -> Value {
    if depth > DEPTH_LIMIT {
        return json!("[redacted: nesting limit]");
    }
    match value {
        Value::String(text) => json!(text_redacted(&text, depth + 1)),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|v| redact(v, inherited_sensitive, depth + 1))
                .collect(),
        ),
        Value::Object(object) => {
            let sensitive = inherited_sensitive
                || ["path", "file_path", "filePath", "filename"]
                    .iter()
                    .filter_map(|k| object.get(*k).and_then(Value::as_str))
                    .any(sensitive_path);
            let request = object.get("id").is_some_and(|v| !v.is_null())
                && object.get("method").is_some_and(Value::is_string);
            let response = object.contains_key("result")
                || (!object.contains_key("method")
                    && object.contains_key("id")
                    && object.contains_key("error"))
                || object.get("type").is_some_and(|v| v == "response");
            let is_file_diff = object
                .get("type")
                .is_some_and(|v| v == "ssmcp.file.diff" || v == "file.diff");
            let mut diff_redacted = false;
            let mut clean = Map::new();
            for (key, value) in object {
                let original_diff = (is_file_diff && matches!(key.as_str(), "diff" | "preview"))
                    .then(|| value.clone());
                let normalized = normalized(&key);
                let value = if secret_key(&key) || environment_key(&key) {
                    json!(REDACTED)
                } else if sensitive && matches!(key.as_str(), "before" | "after") {
                    snapshot_metadata(value)
                } else if sensitive && content_key(&key) {
                    if value.is_null() {
                        Value::Null
                    } else {
                        json!(REDACTED)
                    }
                } else if response && key == "result" {
                    result_metadata(value, depth + 1)
                } else if response && key == "error" && !value.is_null() {
                    error_metadata(value)
                } else if response
                    && matches!(
                        key.as_str(),
                        "content" | "text" | "data" | "output" | "stdout" | "stderr"
                    )
                {
                    // Pi and other transports may expose a response without a
                    // JSON-RPC result envelope. Do not replay its arbitrary body.
                    if value.is_null() {
                        Value::Null
                    } else {
                        json!("[omitted from audit response]")
                    }
                } else if request && key == "params" {
                    request_metadata(value, depth + 1)
                } else if matches!(normalized.as_str(), "command" | "cmd" | "argv" | "args") {
                    command_redacted(value, depth + 1)
                } else if normalized.ends_with("base64") {
                    encoded_redacted(value, sensitive, depth + 1)
                } else {
                    redact(value, sensitive, depth + 1)
                };
                if original_diff.is_some_and(|original| original != value) {
                    diff_redacted = true;
                }
                clean.insert(key, value);
            }
            if diff_redacted {
                clean.insert("diff_redacted".into(), json!(true));
                clean.insert("diff_complete".into(), json!(false));
                clean.insert("diff_unavailable_reason".into(), json!("audit_redaction"));
                for key in ["diff", "preview", "diff_artifact"] {
                    clean.insert(key.into(), Value::Null);
                }
            }
            Value::Object(clean)
        }
        other => other,
    }
}

fn normalized(key: &str) -> String {
    key.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn secret_key(key: &str) -> bool {
    let key = normalized(key);
    matches!(
        key.as_str(),
        "authorization"
            | "proxyauthorization"
            | "cookie"
            | "setcookie"
            | "password"
            | "passwd"
            | "passphrase"
            | "secret"
            | "clientsecret"
            | "privatekey"
            | "apikey"
            | "accesskey"
            | "accesskeyid"
            | "secretaccesskey"
            | "sessiontoken"
            | "accesstoken"
            | "refreshtoken"
            | "idtoken"
            | "token"
            | "credential"
            | "credentials"
            | "logincode"
            | "oauthcode"
    ) || key.ends_with("password")
        || key.ends_with("secret")
        || key.ends_with("privatekey")
        || key.ends_with("accesstoken")
        || key.ends_with("refreshtoken")
        || key.ends_with("apikey")
}

fn environment_key(key: &str) -> bool {
    matches!(
        normalized(key).as_str(),
        "env" | "environment" | "environmentvariables" | "envvars" | "extraenv"
    )
}

fn content_key(key: &str) -> bool {
    matches!(
        normalized(key).as_str(),
        "text"
            | "content"
            | "contents"
            | "body"
            | "data"
            | "database64"
            | "deltabase64"
            | "delta"
            | "diff"
            | "preview"
            | "oldtext"
            | "newtext"
            | "stdout"
            | "stderr"
            | "artifact"
            | "diffartifact"
            | "before"
            | "after"
    )
}

fn sensitive_path(path: &str) -> bool {
    path.split(['/', '\\']).any(|part| {
        let name = part.to_ascii_lowercase();
        matches!(
            name.as_str(),
            ".env"
                | ".envrc"
                | ".ssh"
                | ".aws"
                | ".azure"
                | ".gnupg"
                | ".kube"
                | ".git"
                | ".npmrc"
                | ".netrc"
                | ".pypirc"
                | ".claude.json"
                | "auth.json"
                | "oauth.json"
                | "id_rsa"
                | "id_dsa"
                | "id_ecdsa"
                | "id_ed25519"
        ) || name.starts_with(".env.")
            || name.trim_start_matches('.').starts_with("credential")
            || name.trim_start_matches('.').starts_with("secret")
            || name.starts_with("token.")
            || (name.starts_with("ssh_host_") && name.ends_with("_key"))
            || [".pem", ".key", ".p12", ".pfx", ".jks", ".kdbx"]
                .iter()
                .any(|extension| name.ends_with(extension))
    })
}

fn snapshot_metadata(value: Value) -> Value {
    let state = value
        .get("state")
        .and_then(Value::as_str)
        .filter(|state| matches!(*state, "known" | "missing" | "unknown"))
        .unwrap_or("unknown");
    let reason = value
        .get("reason")
        .and_then(Value::as_str)
        .filter(|reason| {
            matches!(
                *reason,
                "sensitive_path"
                    | "sensitive_content"
                    | "binary_content"
                    | "snapshot_size_limit"
                    | "snapshot_unavailable"
            )
        });
    json!({"state":state,"reason":reason})
}

fn request_metadata(value: Value, depth: usize) -> Value {
    let Some(object) = value.as_object() else {
        return json!({"payload_omitted":true});
    };
    let mut fields = Map::new();
    for key in [
        "name",
        "tool",
        "toolName",
        "server",
        "serverName",
        "path",
        "file_path",
        "cwd",
        "processId",
        "job_id",
        "command",
        "cmd",
        "timeoutMs",
        "timeout_ms",
        "wait_ms",
        "offset",
        "max_bytes",
        "cursor",
        "id",
        "requestId",
        "decision",
        "approval",
        "status",
        "arguments",
        "env",
        "environment",
    ] {
        if let Some(value) = object.get(key) {
            fields.insert(
                key.into(),
                if key == "arguments" {
                    request_metadata(value.clone(), depth + 1)
                } else if matches!(key, "command" | "cmd") {
                    command_redacted(value.clone(), depth + 1)
                } else if environment_key(key) {
                    json!(REDACTED)
                } else {
                    redact(value.clone(), false, depth + 1)
                },
            );
        }
    }
    fields.insert("payload_omitted".into(), json!(true));
    Value::Object(fields)
}

fn result_metadata(value: Value, depth: usize) -> Value {
    let source = value
        .get("structuredContent")
        .filter(|v| v.is_object())
        .unwrap_or(&value);
    let mut fields = Map::new();
    for key in [
        "isError",
        "success",
        "status",
        "state",
        "running",
        "exitCode",
        "exit_code",
        "pid",
        "id",
        "job_id",
        "jobId",
        "session_id",
        "sessionId",
        "duration_ms",
        "durationMs",
        "timedOut",
        "timed_out",
        "cancelled",
        "in_flight",
        "written",
        "bytes",
        "size",
        "offset",
        "next_offset",
        "next_cursor",
        "eof",
        "complete",
        "approval",
        "approval_status",
    ] {
        if let Some(value) = source.get(key).or_else(|| value.get(key)) {
            fields.insert(key.into(), metadata_scalar(value, depth + 1));
        }
    }
    // Explicitly retain the outer MCP failure flag even with structuredContent.
    if let Some(failed) = value.get("isError").and_then(Value::as_bool) {
        fields.insert("isError".into(), json!(failed));
    }
    fields.insert("payload_omitted".into(), json!(true));
    Value::Object(fields)
}

fn metadata_scalar(value: &Value, depth: usize) -> Value {
    match value {
        Value::String(text) => {
            let text = text_redacted(text, depth + 1);
            let mut end = text.len().min(256);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            json!(&text[..end])
        }
        Value::Bool(_) | Value::Number(_) | Value::Null => value.clone(),
        _ => json!("[metadata omitted]"),
    }
}

fn error_metadata(value: Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    let mut fields = Map::new();
    if let Some(code) = value.get("code").filter(|v| v.is_number()) {
        fields.insert("code".into(), code.clone());
    }
    fields.insert("message_omitted".into(), json!(true));
    Value::Object(fields)
}

fn command_redacted(value: Value, depth: usize) -> Value {
    let Value::Array(arguments) = value else {
        return redact(value, false, depth + 1);
    };
    let mut hide_next = false;
    let mut output = Vec::with_capacity(arguments.len());
    for argument in arguments {
        if hide_next {
            output.push(json!(REDACTED));
            hide_next = false;
            continue;
        }
        hide_next = argument.as_str().is_some_and(secret_flag);
        output.push(redact(argument, false, depth + 1));
    }
    Value::Array(output)
}

fn secret_flag(word: &str) -> bool {
    matches!(word, "-u" | "--user")
        || (word.starts_with("--") && secret_key(word.trim_start_matches('-')))
}

fn encoded_redacted(value: Value, sensitive: bool, depth: usize) -> Value {
    if sensitive {
        return json!(REDACTED);
    }
    let Some(encoded) = value.as_str() else {
        return json!(REDACTED);
    };
    let Ok(bytes) = STANDARD.decode(encoded) else {
        return json!(REDACTED);
    };
    let sanitized = bytes_redacted(&bytes, depth + 1);
    if sanitized == bytes {
        // Preserve the original representation when there is no actual match.
        value
    } else {
        json!(STANDARD.encode(sanitized))
    }
}

fn bytes_redacted(bytes: &[u8], depth: usize) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut offset = 0;
    while offset < bytes.len() {
        let remaining = &bytes[offset..];
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                output.extend_from_slice(payload_text_redacted(text, depth + 1).as_bytes());
                break;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                let prefix = std::str::from_utf8(&remaining[..valid]).unwrap();
                output.extend_from_slice(payload_text_redacted(prefix, depth + 1).as_bytes());
                let invalid = error.error_len().unwrap_or(remaining.len() - valid);
                // Invalid and incomplete UTF-8 bytes are data, not replacement
                // characters. Never round-trip them through from_utf8_lossy.
                output.extend_from_slice(&remaining[valid..valid + invalid]);
                offset += valid + invalid;
            }
        }
    }
    output
}

fn payload_text_redacted(text: &str, depth: usize) -> String {
    if depth > DEPTH_LIMIT {
        return "[redacted: nesting limit]".into();
    }
    let trimmed = text.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(value) = serde_json::from_str::<Value>(text)
    {
        let safe = payload_json_redacted(value.clone(), false, depth + 1);
        // An encoded output payload is application data. A field named result
        // is not an RPC response boundary and must not trigger summarization.
        return if safe == value && json_keys_are_unique(text) {
            text.to_owned()
        } else {
            safe.to_string()
        };
    }
    text_redacted(text, depth + 1)
}

fn payload_json_redacted(value: Value, inherited_sensitive: bool, depth: usize) -> Value {
    if depth > DEPTH_LIMIT {
        return json!("[redacted: nesting limit]");
    }
    match value {
        Value::Object(object) => {
            let sensitive = inherited_sensitive
                || ["path", "file_path", "filePath", "filename"]
                    .iter()
                    .filter_map(|key| object.get(*key).and_then(Value::as_str))
                    .any(sensitive_path);
            let mut safe = Map::new();
            for (key, value) in object {
                let value = if secret_key(&key)
                    || environment_key(&key)
                    || (sensitive && content_key(&key))
                {
                    json!(REDACTED)
                } else if normalized(&key).ends_with("base64") {
                    encoded_redacted(value, sensitive, depth + 1)
                } else {
                    payload_json_redacted(value, sensitive, depth + 1)
                };
                safe.insert(key, value);
            }
            Value::Object(safe)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| payload_json_redacted(item, inherited_sensitive, depth + 1))
                .collect(),
        ),
        Value::String(text) => json!(payload_text_redacted(&text, depth + 1)),
        other => other,
    }
}

// Value parsing discards earlier duplicate keys. Only restore original bytes
// when no object contains aliases that could hide a discarded secret value.
fn json_keys_are_unique(text: &str) -> bool {
    let mut objects = Vec::<std::collections::BTreeSet<String>>::new();
    let bytes = text.as_bytes();
    let mut at = 0;
    while let Some(byte) = bytes.get(at) {
        match byte {
            b'{' => objects.push(std::collections::BTreeSet::new()),
            b'}' => {
                objects.pop();
            }
            b'"' => {
                let start = at;
                at += 1;
                while let Some(byte) = bytes.get(at) {
                    match byte {
                        b'\\' => at += 2,
                        b'"' => break,
                        _ => at += 1,
                    }
                }
                let end = at + 1;
                if end > text.len() {
                    return false;
                }
                if text[end..].trim_start().starts_with(':') {
                    let Ok(key) = serde_json::from_str::<String>(&text[start..end]) else {
                        return false;
                    };
                    if !objects.last_mut().is_some_and(|keys| keys.insert(key)) {
                        return false;
                    }
                }
            }
            _ => {}
        }
        at += 1;
    }
    true
}

fn text_redacted(text: &str, depth: usize) -> String {
    if depth > DEPTH_LIMIT {
        return "[redacted: nesting limit]".into();
    }
    if text.contains("-----BEGIN") && text.contains("PRIVATE KEY-----") {
        return "[redacted private key]".into();
    }
    let trimmed = text.trim_start();
    if (trimmed.starts_with('{') || trimmed.starts_with('['))
        && let Ok(value) = serde_json::from_str::<Value>(text)
    {
        let safe = redact(value.clone(), false, depth + 1);
        return if safe == value && json_keys_are_unique(text) {
            text.to_owned()
        } else {
            safe.to_string()
        };
    }
    let mut output = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let body = line.strip_suffix('\n').unwrap_or(line);
        let mut hide_from = None;
        for (index, ch) in body.char_indices() {
            if !matches!(ch, '=' | ':') {
                continue;
            }
            let prefix = body[..index].trim_end().trim_end_matches(['"', '\'']);
            let start = prefix
                .char_indices()
                .rfind(|(_, ch)| !ch.is_ascii_alphanumeric() && !matches!(*ch, '_' | '-'))
                .map_or(0, |(index, ch)| index + ch.len_utf8());
            let key = &prefix[start..];
            let environment = key.len() >= 2
                && key.len() <= 128
                && key.bytes().any(|c| c.is_ascii_uppercase())
                && key
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == b'_');
            if secret_key(key) || (ch == '=' && environment) {
                let offset = index + ch.len_utf8();
                hide_from = Some(offset);
                break;
            }
        }
        if hide_from.is_none() {
            let mut pending = false;
            for (start, word) in words(body) {
                if pending {
                    hide_from = Some(start);
                    break;
                }
                pending = secret_flag(word) || word.eq_ignore_ascii_case("bearer");
            }
        }
        let visible = if let Some(index) = hide_from {
            format!("{}{REDACTED}", &body[..index])
        } else {
            body.to_owned()
        };
        output.push_str(&tokens_redacted(&urls_redacted(&visible)));
        if line.ends_with('\n') {
            output.push('\n');
        }
    }
    output
}

fn words(text: &str) -> Vec<(usize, &str)> {
    let mut result = Vec::new();
    let mut start = None;
    for (index, ch) in text.char_indices() {
        if ch.is_whitespace() {
            if let Some(begin) = start.take() {
                result.push((begin, &text[begin..index]));
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(begin) = start {
        result.push((begin, &text[begin..]));
    }
    result
}

fn tokens_redacted(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut start = None;
    let mut copied = 0;
    for (index, ch) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
            if start.is_none() {
                start = Some(index);
            }
            continue;
        }
        let Some(begin) = start.take() else { continue };
        let word = &text[begin..index];
        let secret = (word.len() >= 8
            && [
                "sk-",
                "ghp_",
                "github_pat_",
                "gho_",
                "ghu_",
                "ghs_",
                "ghr_",
                "xoxb-",
                "xoxp-",
                "xoxa-",
                "xoxr-",
            ]
            .iter()
            .any(|p| word.starts_with(p)))
            || (word.len() == 20
                && (word.starts_with("AKIA") || word.starts_with("ASIA"))
                && word
                    .bytes()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit()))
            || (word.starts_with("eyJ") && word.len() >= 32 && word.matches('.').count() == 2);
        if secret {
            output.push_str(&text[copied..begin]);
            output.push_str(REDACTED);
            copied = index;
        }
    }
    output.push_str(&text[copied..]);
    output
}

fn urls_redacted(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut copied = 0;
    let mut cursor = 0;
    while let Some(found) = text[cursor..].find("://") {
        let begin = cursor + found + 3;
        let end = text[begin..]
            .find(|c: char| c.is_whitespace() || matches!(c, '/' | '"' | '\''))
            .map_or(text.len(), |i| begin + i);
        if let Some(at) = text[begin..end].rfind('@') {
            output.push_str(&text[copied..begin]);
            output.push_str(REDACTED);
            copied = begin + at;
        }
        cursor = end.max(begin);
        if cursor == text.len() {
            break;
        }
    }
    output.push_str(&text[copied..]);
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_text_preserves_source_only_when_no_redaction_is_needed() {
        for source in [
            r#"  { "z": 1, "a": "\t" }  "#,
            r#"[ { "z": true, "a": "界" }, { "z": false, "a": "text" } ]"#,
        ] {
            assert_eq!(event(json!(source)), json!(source));
        }
        let source =
            r#"{ "z": 1, "nested": { "api\u004bey": "fixture-private-value" }, "a": "\t" }"#;
        let safe = event(json!(source));
        let text = safe.as_str().unwrap();
        assert!(!text.contains("fixture-private-value"), "{text}");
        let value: Value = serde_json::from_str(text).unwrap();
        assert_eq!(value["nested"]["apiKey"], REDACTED);
        assert_eq!(value["z"], 1);
        assert_eq!(value["a"], "\t");

        let duplicate =
            r#"{ "item": {"apiKey": "discarded-private-value"}, "i\u0074em": {"ok": true} }"#;
        for text in [
            event(json!(duplicate)).as_str().unwrap().to_owned(),
            payload_text_redacted(duplicate, 0),
        ] {
            assert!(!text.contains("discarded-private-value"), "{text}");
            assert_eq!(
                serde_json::from_str::<Value>(&text).unwrap()["item"]["ok"],
                true
            );
        }
    }

    #[test]
    fn nested_credentials_environment_and_private_file_content_are_removed() {
        let source = json!({
            "type":"test","env":{"PATH":"/private/home","UNUSUAL":"unmarked-environment-secret"},
            "headers":{"Authorization":"Bearer sensitive123","Cookie":"session=secret"},
            "nested":{"apiKey":"api-secret","private_key":"key-secret"},
            "files":[{"path":".ssh/id_ed25519","text":"arbitrary-key-body","data_base64":STANDARD.encode("hidden")}],
            "login_code":"human-login-secret"
        });
        let audit = event(source.clone()).to_string();
        for secret in [
            "/private/home",
            "unmarked-environment-secret",
            "sensitive123",
            "session=secret",
            "api-secret",
            "key-secret",
            "arbitrary-key-body",
            "human-login-secret",
            &STANDARD.encode("hidden"),
        ] {
            assert!(!audit.contains(secret), "{audit}");
        }
        assert_eq!(source["nested"]["apiKey"], "api-secret");
    }

    #[test]
    fn native_responses_keep_status_without_replaying_bodies_or_error_messages() {
        let result = event(
            json!({"id":"call","result":{"isError":true,"structuredContent":{
            "exit_code":1,"job_id":"one","stdout":"duplicate-output","text":"private-file-body",
            "environment":{"secret":"abc"},"unknown":{"secret":"anything"}}}}),
        );
        assert_eq!(result["result"]["exit_code"], 1);
        assert_eq!(result["result"]["isError"], true);
        assert_eq!(result["result"]["job_id"], "one");
        assert!(!result.to_string().contains("duplicate-output"));
        assert!(!result.to_string().contains("private-file-body"));
        let error =
            event(json!({"id":1,"error":{"code":-32000,"message":"arbitrary-sensitive-content"}}));
        assert_eq!(error["error"]["code"], -32000);
        assert!(!error.to_string().contains("arbitrary-sensitive-content"));
    }

    #[test]
    fn request_logs_keep_routing_and_redact_command_flags_and_payloads() {
        let value = event(
            json!({"id":1,"method":"tools/call","params":{"name":"run","arguments":{
            "command":["curl","--token","unguessable-value","--password=another-secret"],
            "env":{"SECRET":"third-secret"},"path":"src/a.rs","text":"private source body"}}}),
        );
        assert_eq!(value["params"]["name"], "run");
        assert_eq!(value["params"]["arguments"]["command"][1], "--token");
        assert_eq!(value["params"]["arguments"]["command"][2], REDACTED);
        for secret in [
            "unguessable-value",
            "another-secret",
            "third-secret",
            "private source body",
        ] {
            assert!(!value.to_string().contains(secret));
        }
    }

    #[test]
    fn streams_keep_regular_text_and_redact_both_text_and_encoded_secret_copies() {
        let text = "progress 1\nAPI_TOKEN=ordinary-secret\nsk-proj-example-secret-material\n";
        let value = event(json!({"type":"exec.stdout","job_id":"7","text":text,
            "params":{"deltaBase64":STANDARD.encode(text),"stream":"stdout","processId":"7"}}));
        let decoded = String::from_utf8(
            STANDARD
                .decode(value["params"]["deltaBase64"].as_str().unwrap())
                .unwrap(),
        )
        .unwrap();
        assert!(decoded.contains("progress 1"));
        assert!(!decoded.contains("ordinary-secret"));
        assert!(!decoded.contains("sk-proj-example-secret-material"));
        assert!(!value.to_string().contains("ordinary-secret"));
    }

    #[test]
    fn free_text_and_private_keys_are_redacted_without_losing_nonsecret_values() {
        for text in [
            "Authorization: Bearer raw-access-value",
            "command --password 'secret with spaces'",
            "https://user:password@example.test/path",
            "-----BEGIN OPENSSH PRIVATE KEY-----\nprivate-body\n-----END OPENSSH PRIVATE KEY-----",
        ] {
            let clean = text_redacted(text, 0);
            for secret in [
                "raw-access-value",
                "secret with spaces",
                "user:password",
                "private-body",
            ] {
                assert!(!clean.contains(secret), "{clean}");
            }
        }
        assert_eq!(
            text_redacted("exit=0\nnormal output\n", 0),
            "exit=0\nnormal output\n"
        );
        assert_eq!(
            event(json!({"usage":{"input_tokens":12,"output_tokens":4}}))["usage"]["input_tokens"],
            12
        );
    }

    #[test]
    fn utf8_key_boundaries_preserve_plain_text_and_redact_secrets() {
        // The first case places the three-byte delimiter at byte 51. Advancing
        // by one byte instead of one char used to panic at start byte 52.
        for delimiter in ['\u{3001}', '\u{1f680}', '\u{00e9}', '\u{3000}'] {
            let prefix = format!("{}{delimiter}", "a".repeat(51));
            let plain = format!("{prefix}exit=0");
            assert_eq!(text_redacted(&plain, 0), plain);
            let stream = event(json!({
                "type":"exec.stdout", "job_id":"utf8-regression", "text":plain,
                "params":{"deltaBase64":STANDARD.encode(plain.as_bytes()), "stream":"stdout"}
            }));
            assert_eq!(stream["text"], plain);
            assert_eq!(
                STANDARD
                    .decode(stream["params"]["deltaBase64"].as_str().unwrap())
                    .unwrap(),
                plain.as_bytes()
            );

            let secret = format!("{prefix}API_TOKEN=ordinary-secret-value");
            let expected = format!("{prefix}API_TOKEN={REDACTED}");
            assert_eq!(text_redacted(&secret, 0), expected);
            let stream = event(json!({
                "type":"exec.stderr", "job_id":"utf8-regression", "text":secret,
                "params":{"deltaBase64":STANDARD.encode(secret.as_bytes()), "stream":"stderr"}
            }));
            assert_eq!(stream["text"], expected);
            assert_eq!(
                STANDARD
                    .decode(stream["params"]["deltaBase64"].as_str().unwrap())
                    .unwrap(),
                expected.as_bytes()
            );
            assert!(!stream.to_string().contains("ordinary-secret-value"));
        }
    }

    #[test]
    fn repeated_audit_boundaries_are_idempotent() {
        let original = json!({"response":{"id":1,"result":{"status":"ok","text":"private"}},"env":{"A":"secret"},
            "command":["tool","--token","secret"],"data_base64":STANDARD.encode("PASSWORD=secret\n")});
        let first = event(original);
        assert_eq!(event(first.clone()), first);
    }
    #[test]
    fn encoded_binary_is_exact_without_a_secret_match() {
        for bytes in [
            vec![0, 255],
            vec![0xf0, 0x9f, 0x92],
            br#" { "result": { "text": "ordinary application data" } } "#.to_vec(),
        ] {
            let source = json!({"kind":"binary","data_base64":STANDARD.encode(&bytes)});
            assert_eq!(event(source.clone()), source);
        }
    }

    #[test]
    fn mixed_binary_only_changes_identified_secret_bytes() {
        let bytes = b"\0\xffsk-proj-private-token-material\xfe\0";
        let safe = event(json!({"data_base64":STANDARD.encode(bytes)}));
        let decoded = STANDARD
            .decode(safe["data_base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(&decoded[..2], b"\0\xff");
        assert_eq!(&decoded[decoded.len() - 2..], b"\xfe\0");
        assert!(
            decoded
                .windows(REDACTED.len())
                .any(|window| window == REDACTED.as_bytes())
        );
        assert!(
            !decoded
                .windows(b"private-token-material".len())
                .any(|window| window == b"private-token-material")
        );
    }
}
