//! Private, bounded JSON frames carried over outbound HTTPS/WebSocket or SSH connections.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};

/// The sole supported wire protocol, including controller, source audit and steering.
pub const VERSION: u32 = 7;
pub const MAX_FRAME: usize = 4 * 1024 * 1024;
pub const MAX_READ: usize = 32 * 1024;
pub const MAX_WRITE: usize = 16 * 1024;
pub const MAX_WAIT_MS: u64 = 5_000;
pub const MAX_INFLIGHT: usize = 16;
pub const MAX_CLIENTS_PER_USER: usize = 128;
pub const MAX_ALLOWED_PRINCIPALS: usize = 32;
/// Opted into through the authenticated WebSocket HTTP handshake, never display metadata.
pub const SESSION_ACCESS_CAPABILITY: &str = "session_access_v1";
pub const CAPABILITIES_HEADER: &str = "ssmcp-capabilities";
pub const MAX_TITLE_BYTES: usize = 512;
pub const MAX_CODE_BYTES: usize = 64 * 1024;
/// Shared per-client context budget for native and orchestrated results.
pub(crate) const MAX_CONTEXT_ENTRY_BYTES: usize = 12 * 1024;
pub const MAX_USER_MESSAGE_BYTES: usize = 16 * 1024;
pub const MAX_USER_MESSAGES: usize = 64;

/// Local operator commands act only on the connection that carries the frame.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum SteeringCommand {
    Snapshot {},
    Publish {
        command_id: String,
        message_id: String,
        expected_revision: u64,
        text: String,
        /// `next` or an exact registered collaboration agent ID.
        target: String,
    },
    Takeback {
        command_id: String,
        message_id: String,
        expected_revision: u64,
    },
    Cancel {
        command_id: String,
        message_id: String,
        expected_revision: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SteeringAgent {
    pub agent_id: String,
    pub name: String,
    pub role: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SteeringState {
    Queued,
    Editing,
    Delivered,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SteeringDelivery {
    pub run_id: String,
    pub agent_id: Option<String>,
    /// Inclusion in an HTTP response, not proof of reading or execution.
    pub included_at_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SteeringMessage {
    pub id: String,
    pub revision: u64,
    pub text: String,
    pub target: String,
    pub state: SteeringState,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    pub delivery: Option<SteeringDelivery>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SteeringSnapshot {
    pub schema_version: u32,
    pub client_id: String,
    pub revision: u64,
    pub agents: Vec<SteeringAgent>,
    pub messages: Vec<SteeringMessage>,
}

/// Immutable source of the orchestration that dispatched this native operation.
/// This is display metadata, never a tool argument or an authorization grant.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CodeContext {
    pub run_id: String,
    pub code: String,
    pub timestamp_ms: u64,
}

impl CodeContext {
    pub fn validate(&self) -> Result<(), String> {
        validate_run_id(&self.run_id)?;
        if self.code.trim().is_empty() || self.code.len() > MAX_CODE_BYTES {
            return Err("code audit source must contain 1..=65536 UTF-8 bytes".into());
        }
        Ok(())
    }
}

/// Run completion is distinct from any individual native call or command job.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CodeCompletion {
    pub run_id: String,
    pub success: bool,
    pub status: String,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl CodeCompletion {
    pub fn validate(&self) -> Result<(), String> {
        validate_run_id(&self.run_id)?;
        if !matches!(self.status.as_str(), "completed" | "failed" | "timeout")
            || self.success != (self.status == "completed")
            || self.error.as_ref().is_some_and(|error| error.len() > 4096)
        {
            return Err("invalid code audit completion".into());
        }
        Ok(())
    }
}

fn validate_run_id(run_id: &str) -> Result<(), String> {
    if run_id.len() != 32 || !run_id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid code audit run ID".into());
    }
    Ok(())
}

/// Metadata and requested access policy supplied by the authenticated local client.
/// These fields never select or replace the server-authenticated account.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ClientMetadata {
    pub name: String,
    /// Operator-supplied task context, never an authorization policy or command.
    /// Omit when unset so clients without a title retain the legacy wire shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Absolute working directory at client startup; agent sessions may use other directories.
    pub cwd: String,
    pub hostname: String,
    /// Canonical server usernames or GitHub principals allowed to access this connection.
    /// Empty retains connection-bound login-code authorization.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allow: Vec<String>,
}

impl ClientMetadata {
    pub fn validate(&self) -> Result<(), String> {
        validate_title(self.title.as_deref())?;
        for (label, value, limit) in [
            ("name", self.name.as_str(), 128),
            ("cwd", self.cwd.as_str(), 4096),
            ("hostname", self.hostname.as_str(), 255),
        ] {
            if value.trim().is_empty() || value.len() > limit || value.chars().any(char::is_control)
            {
                return Err(format!(
                    "client {label} must be nonempty, contain no control characters, and be at most {limit} bytes"
                ));
            }
        }
        if !absolute_client_path(&self.cwd) {
            return Err("client cwd must be an absolute path".into());
        }
        if self.allow.len() > MAX_ALLOWED_PRINCIPALS {
            return Err(format!(
                "client allow list must contain at most {MAX_ALLOWED_PRINCIPALS} principals"
            ));
        }
        for (index, principal) in self.allow.iter().enumerate() {
            if canonical_allow_principal(principal)? != *principal {
                return Err(
                    "client allow entries must use lowercase server usernames or username@github"
                        .into(),
                );
            }
            if self.allow[..index].contains(principal) {
                return Err("client allow list must not contain duplicate principals".into());
            }
        }
        Ok(())
    }
}

// The relay validates client metadata on Linux, including native Windows paths.
// This is display metadata; it never resolves a path on the relay host.
fn absolute_client_path(path: &str) -> bool {
    if path.starts_with('/') {
        return true;
    }
    let path = path.strip_prefix(r"\\?\").unwrap_or(path);
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'/' | b'\\')
    {
        return true;
    }
    let unc = path
        .strip_prefix(r"\\")
        .or_else(|| path.strip_prefix(r"UNC\"));
    if let Some(unc) = unc {
        let mut parts = unc.split(['/', '\\']);
        return parts
            .next()
            .is_some_and(|p| !p.is_empty() && p != "." && p != "?")
            && parts.next().is_some_and(|p| !p.is_empty());
    }
    false
}

pub fn validate_title(title: Option<&str>) -> Result<(), String> {
    if let Some(title) = title
        && (title.trim().is_empty()
            || title.len() > MAX_TITLE_BYTES
            || title.chars().any(char::is_control))
    {
        return Err(format!(
            "session title must be nonempty, contain no control characters, and be at most {MAX_TITLE_BYTES} UTF-8 bytes"
        ));
    }
    Ok(())
}

/// Startup allow targets use separate server-name and GitHub-login namespaces.
pub fn canonical_allow_principal(principal: &str) -> Result<String, String> {
    if principal.contains('@') {
        canonical_github_principal(principal)
    } else {
        crate::username::normalize(principal)
            .map_err(|error| format!("invalid server username: {error}"))
    }
}

/// Validate a GitHub principal and return its case-insensitive canonical form.
pub fn canonical_github_principal(principal: &str) -> Result<String, String> {
    let (username, provider) = principal
        .split_once('@')
        .ok_or("allow entries must use username@github")?;
    if !provider.eq_ignore_ascii_case("github") {
        return Err("allow entries must use username@github; only GitHub is supported".into());
    }
    if username.is_empty()
        || username.len() > 39
        || username.starts_with('-')
        || username.ends_with('-')
        || username.contains("--")
        || !username
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("invalid GitHub username: use 1..=39 ASCII letters or digits, with optional single hyphens between them".into());
    }
    Ok(format!("{}@github", username.to_ascii_lowercase()))
}

/// Display attribution supplied by a caller. It never grants authority or proves
/// which model ran; MCP clientInfo identifies an application, not its model.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Controller {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub source: ControllerSource,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ControllerSource {
    CallerReported,
    McpClientInfo,
}

impl Controller {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.is_none() && self.model.is_none() {
            return Err("controller requires a name or model".into());
        }
        for (field, value) in [("name", &self.name), ("model", &self.model)] {
            if let Some(value) = value
                && (value.trim().is_empty()
                    || value.len() > 128
                    || value.chars().any(char::is_control))
            {
                return Err(format!(
                    "controller {field} must be 1..=128 UTF-8 bytes without control characters"
                ));
            }
        }
        if self.source == ControllerSource::McpClientInfo && self.model.is_some() {
            return Err("MCP clientInfo cannot attest a model".into());
        }
        Ok(())
    }
}

/// Public discovery information. IDs identify a connection, not a display name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ClientInfo {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(rename = "type")]
    pub mode: Mode,
    pub cwd: String,
    pub hostname: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Shell,
    Codex,
    Cc,
    Pi,
    Opencode,
}

impl Mode {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "shell" => Ok(Self::Shell),
            "codex" => Ok(Self::Codex),
            "cc" => Ok(Self::Cc),
            "pi" => Ok(Self::Pi),
            "opencode" => Ok(Self::Opencode),
            _ => Err("-t must be shell, codex, cc, pi, or opencode".into()),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Shell => "shell",
            Self::Codex => "codex",
            Self::Cc => "cc",
            Self::Pi => "pi",
            Self::Opencode => "opencode",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Read {
        #[serde(default)]
        cursor: u64,
        #[serde(default = "default_max_bytes")]
        max_bytes: usize,
        #[serde(default)]
        wait_ms: u64,
    },
    Write {
        data: String,
    },
    Resize {
        rows: u16,
        cols: u16,
    },
    Status {},
    ListTools {
        #[serde(default)]
        cursor: Option<String>,
    },
    CallTool {
        name: String,
        arguments: Value,
    },
}

fn default_max_bytes() -> usize {
    16 * 1024
}

impl Operation {
    pub fn validate(&self) -> std::result::Result<(), String> {
        match self {
            Self::Read {
                max_bytes, wait_ms, ..
            } if *max_bytes == 0 || *max_bytes > MAX_READ || *wait_ms > MAX_WAIT_MS => Err(
                format!("max_bytes must be 1..={MAX_READ}; wait_ms must be 0..={MAX_WAIT_MS}"),
            ),
            Self::Write { data } if data.len() > MAX_WRITE => {
                Err(format!("data exceeds {MAX_WRITE} bytes"))
            }
            Self::Resize { rows, cols }
                if *rows == 0 || *cols == 0 || *rows > 1000 || *cols > 1000 =>
            {
                Err("rows and cols must be 1..=1000".into())
            }
            Self::ListTools {
                cursor: Some(cursor),
            } if cursor.len() > 128 => Err("tool cursor is too long".into()),
            Self::CallTool { name, arguments }
                if name.is_empty()
                    || name.len() > 128
                    || !name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_.-".contains(&b))
                    || !arguments.is_object() =>
            {
                Err("invalid tool name or arguments".into())
            }
            _ => Ok(()),
        }
    }
}

/// Requests from the authenticated local connection; no target session or actor is accepted.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum AccessCommand {
    List {},
    Add { principal: String },
    Remove { principal: String },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccessEntry {
    pub principal: String,
    pub login: String,
    pub subject: String,
    pub owner: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct AccessSnapshot {
    pub entries: Vec<AccessEntry>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl AccessSnapshot {
    pub fn validate(&self) -> Result<(), String> {
        if self.entries.is_empty()
            || self.entries.len() > MAX_ALLOWED_PRINCIPALS
            || self.entries.iter().filter(|entry| entry.owner).count() != 1
            || self
                .message
                .as_ref()
                .is_some_and(|message| message.len() > 512)
        {
            return Err("invalid session access snapshot".into());
        }
        let mut subjects = std::collections::HashSet::new();
        let mut principals = std::collections::HashSet::new();
        for entry in &self.entries {
            if canonical_github_principal(&entry.principal)? != entry.principal
                || format!("{}@github", entry.login.to_ascii_lowercase()) != entry.principal
                || entry.subject.is_empty()
                || entry.subject.len() > 20
                || !entry.subject.bytes().all(|byte| byte.is_ascii_digit())
                || !subjects.insert(&entry.subject)
                || !principals.insert(&entry.principal)
            {
                return Err("invalid session access identity".into());
            }
        }
        Ok(())
    }
}

// Do not derive Debug: Ready carries a one-time OAuth login code.
#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    Hello {
        version: u32,
        mode: Mode,
        metadata: ClientMetadata,
    },
    Ready {
        url: String,
        login_code: String,
        login_expires_in: u64,
        client_id: String,
        // Omitted for peers that did not opt in through the WebSocket handshake.
        // Strict v7 clients must receive exactly the original Ready field set.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        capabilities: Vec<String>,
    },
    Access {
        id: u64,
        command: AccessCommand,
    },
    AccessReply {
        id: u64,
        result: Result<AccessSnapshot, String>,
    },
    Steering {
        id: u64,
        command: SteeringCommand,
    },
    SteeringReply {
        id: u64,
        result: Result<SteeringSnapshot, String>,
    },
    Call {
        id: u64,
        operation: Operation,
        /// Optional per-call attribution prevents concurrent callers from
        /// overwriting one another's displayed identity.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        controller: Option<Controller>,
        /// Present only when this call belongs to an audited code-mode run.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        code_context: Option<CodeContext>,
    },
    CodeModeFinished {
        completion: CodeCompletion,
    },
    Reply {
        id: u64,
        result: std::result::Result<Value, String>,
    },
    /// Confirms that the broker received a successful client reload ACK.
    /// Only sent for an explicitly requested, accepted reload operation.
    ReloadCommit {
        id: u64,
    },
    Reject {
        message: String,
    },
}

pub async fn read_frame<R: AsyncBufRead + Unpin>(reader: &mut R) -> io::Result<Option<Frame>> {
    let mut line = Vec::new();
    loop {
        let data = reader.fill_buf().await?;
        if data.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "incomplete frame",
                ))
            };
        }
        let n = data
            .iter()
            .position(|b| *b == b'\n')
            .map_or(data.len(), |i| i + 1);
        if line.len() + n > MAX_FRAME {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame exceeds limit",
            ));
        }
        let complete = data[n - 1] == b'\n';
        line.extend_from_slice(&data[..n]);
        reader.consume(n);
        if complete {
            return serde_json::from_slice(&line)
                .map(Some)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e));
        }
    }
}

pub async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, frame: &Frame) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(frame)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame exceeds limit",
        ));
    }
    writer.write_all(&bytes).await?;
    writer.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn controller_fields_are_display_only_and_bounded() {
        let mut controller = Controller {
            name: Some("ChatGPT".into()),
            model: Some("example-model".into()),
            source: ControllerSource::CallerReported,
        };
        assert!(controller.validate().is_ok());
        controller.source = ControllerSource::McpClientInfo;
        assert!(controller.validate().is_err());
        controller.model = None;
        assert!(controller.validate().is_ok());
        for name in [
            "".into(),
            " \t".into(),
            "bad\u{1b}[2J".into(),
            "x".repeat(129),
        ] {
            controller.name = Some(name);
            assert!(controller.validate().is_err());
        }
        controller.name = None;
        assert!(controller.validate().is_err());
    }

    #[test]
    fn call_metadata_is_optional_and_keeps_per_call_attribution() {
        let frame = Frame::Call {
            id: 7,
            operation: Operation::Status {},
            controller: None,
            code_context: None,
        };
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"type":"call","id":7,"operation":{"op":"status"}})
        );
        let Frame::Call { controller, .. } = serde_json::from_value::<Frame>(value).unwrap() else {
            panic!("expected a call");
        };
        assert!(controller.is_none());
        let frame = Frame::Call {
            id: 8,
            operation: Operation::Status {},
            controller: Some(Controller {
                name: None,
                model: Some("model-a".into()),
                source: ControllerSource::CallerReported,
            }),
            code_context: None,
        };
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["controller"]["source"], "caller-reported");
        assert_eq!(value["controller"]["model"], "model-a");
    }

    #[test]
    fn access_capability_preserves_strict_legacy_ready_and_rejects_forged_scope() {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct LegacyReady {
            #[serde(rename = "type")]
            kind: String,
            url: String,
            login_code: String,
            login_expires_in: u64,
            client_id: String,
        }
        let legacy = serde_json::json!({"type":"ready","url":"https://ssmcp.test/mcp","login_code":"","login_expires_in":0,"client_id":"fixture"});
        let frame: Frame = serde_json::from_value(legacy.clone()).unwrap();
        let Frame::Ready {
            ref capabilities, ..
        } = frame
        else {
            panic!("ready");
        };
        assert!(capabilities.is_empty());
        assert_eq!(serde_json::to_value(frame).unwrap(), legacy);
        let parsed: LegacyReady = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(
            (
                parsed.kind.as_str(),
                parsed.url.as_str(),
                parsed.login_code.as_str(),
                parsed.login_expires_in,
                parsed.client_id.as_str()
            ),
            ("ready", "https://ssmcp.test/mcp", "", 0, "fixture")
        );
        let mut upgraded = legacy;
        upgraded["capabilities"] = serde_json::json!([SESSION_ACCESS_CAPABILITY]);
        assert!(serde_json::from_value::<Frame>(upgraded.clone()).is_ok());
        assert!(serde_json::from_value::<LegacyReady>(upgraded).is_err());
        for command in [
            serde_json::json!({"action":"list","client_id":"another-session"}),
            serde_json::json!({"action":"add","principal":"bob@github","subject":"42"}),
            serde_json::json!({"action":"remove","principal":"alice@github","controller":{"name":"owner"}}),
        ] {
            assert!(serde_json::from_value::<AccessCommand>(command).is_err());
        }
        assert!(serde_json::from_value::<Frame>(serde_json::json!({"type":"access","id":1,"command":{"action":"list"},"client_id":"another"})).is_err());
    }

    #[tokio::test]
    async fn bounded_frames_reject_truncation_and_oversize() {
        let mut truncated = &b"{\"type\":\"hello\",\"version\":1}"[..];
        assert_eq!(
            read_frame(&mut truncated).await.err().unwrap().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let huge = vec![b'x'; MAX_FRAME + 1];
        assert_eq!(
            read_frame(&mut huge.as_slice()).await.err().unwrap().kind(),
            io::ErrorKind::InvalidData
        );
        assert!(read_frame(&mut &b""[..]).await.unwrap().is_none());
    }

    #[test]
    fn code_source_metadata_is_bounded_exact_and_separate_from_tool_arguments() {
        let code = format!("//{}\n", "x".repeat(MAX_CODE_BYTES - 3));
        let context = CodeContext {
            run_id: "a".repeat(32),
            code: code.clone(),
            timestamp_ms: 42,
        };
        assert!(context.validate().is_ok());
        let frame = Frame::Call {
            id: 1,
            operation: Operation::CallTool {
                name: "tool.run".into(),
                arguments: serde_json::json!({"value":3}),
            },
            controller: None,
            code_context: Some(context.clone()),
        };
        let value = serde_json::to_value(frame).unwrap();
        assert_eq!(value["code_context"]["code"], code);
        assert_eq!(
            value["operation"]["arguments"],
            serde_json::json!({"value":3})
        );
        let Frame::Call {
            code_context: Some(decoded),
            ..
        } = serde_json::from_value(value).unwrap()
        else {
            panic!("missing code context");
        };
        assert_eq!(decoded, context);
        assert!(
            CodeContext {
                code: "x".repeat(MAX_CODE_BYTES + 1),
                ..context.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            CodeContext {
                run_id: "bad".into(),
                ..context
            }
            .validate()
            .is_err()
        );
        let completion = CodeCompletion {
            run_id: "a".repeat(32),
            success: false,
            status: "timeout".into(),
            duration_ms: 30_000,
            error: None,
        };
        assert!(completion.validate().is_ok());
        assert!(
            CodeCompletion {
                success: true,
                ..completion
            }
            .validate()
            .is_err()
        );
    }
    #[tokio::test]
    async fn frames_round_trip() {
        let mut out = Vec::new();
        write_frame(
            &mut out,
            &Frame::Hello {
                version: VERSION,
                mode: Mode::Shell,
                metadata: ClientMetadata {
                    name: "dev".into(),
                    title: None,
                    cwd: "/project".into(),
                    hostname: "localhost".into(),
                    allow: Vec::new(),
                },
            },
        )
        .await
        .unwrap();
        assert!(matches!(
            read_frame(&mut out.as_slice()).await.unwrap(),
            Some(Frame::Hello {
                version: VERSION,
                mode: Mode::Shell,
                metadata: ClientMetadata { name, title, cwd, hostname, allow },
            }) if name == "dev" && title.is_none() && cwd == "/project" && hostname == "localhost" && allow.is_empty()
        ));
    }
    #[test]
    fn client_paths_are_validated_independently_of_relay_os() {
        for path in [
            "/home/alice/project",
            r"C:\Users\Alice\project",
            r"C:/work/project",
            r"\\server\share\project",
            r"\\?\C:\work",
        ] {
            assert!(absolute_client_path(path), "{path}");
        }
        for path in ["project", r"C:project", r"\work", r"\\server", r"\\.\pipe"] {
            assert!(!absolute_client_path(path), "{path}");
        }
    }

    #[test]
    fn client_metadata_rejects_relative_paths_controls_and_unbounded_fields() {
        let valid = ClientMetadata {
            name: "My workstation".into(),
            title: None,
            cwd: "/home/user/project".into(),
            hostname: "workstation".into(),
            allow: Vec::new(),
        };
        assert!(valid.validate().is_ok());
        for cwd in [
            r"C:\Users\developer\project",
            "D:/work/project",
            r"\\server\share\project",
            r"\\?\C:\work",
        ] {
            let metadata = ClientMetadata {
                cwd: cwd.into(),
                ..valid.clone()
            };
            let decoded: ClientMetadata =
                serde_json::from_value(serde_json::to_value(&metadata).unwrap()).unwrap();
            assert!(decoded.validate().is_ok(), "rejected client path {cwd}");
        }
        for name in ["".into(), "  ".into(), "bad\nname".into(), "x".repeat(129)] {
            assert!(
                ClientMetadata {
                    name,
                    ..valid.clone()
                }
                .validate()
                .is_err()
            );
        }
        for cwd in [
            "relative".into(),
            "C:relative".into(),
            r"\relative".into(),
            r"\\server".into(),
            "/bad\0path".into(),
            format!("/{}", "x".repeat(4096)),
        ] {
            assert!(
                ClientMetadata {
                    cwd,
                    ..valid.clone()
                }
                .validate()
                .is_err()
            );
        }
        assert!(
            ClientMetadata {
                hostname: "x".repeat(256),
                ..valid
            }
            .validate()
            .is_err()
        );
    }
    #[test]
    fn github_principals_enforce_username_rules_and_normalize_case() {
        for (input, expected) in [
            ("OctoCat@GitHub", "octocat@github"),
            ("a@github", "a@github"),
            ("9-Dev@github", "9-dev@github"),
        ] {
            assert_eq!(canonical_github_principal(input).unwrap(), expected);
        }
        assert!(canonical_github_principal(&format!("{}@github", "a".repeat(39))).is_ok());
        assert!(canonical_github_principal(&format!("{}@github", "a".repeat(40))).is_err());
        for input in [
            "",
            "alice",
            "@github",
            "alice@",
            "alice@example.com",
            "alice@github.com",
            "alice@github@github",
            "-alice@github",
            "alice-@github",
            "alice--dev@github",
            "alice_dev@github",
            "alice.dev@github",
            "álìce@github",
            "alice\n@github",
            " alice@github",
        ] {
            assert!(canonical_github_principal(input).is_err(), "{input:?}");
        }
    }

    #[test]
    fn allow_principals_distinguish_server_usernames_from_github_logins() {
        for (input, expected) in [
            ("Alice", "alice"),
            ("Alice_2-home", "alice_2-home"),
            ("Alice@GitHub", "alice@github"),
            ("a@github", "a@github"),
            ("root@github", "root@github"),
            ("gh-123@github", "gh-123@github"),
        ] {
            assert_eq!(canonical_allow_principal(input).unwrap(), expected);
        }
        assert!(canonical_allow_principal(&"a".repeat(32)).is_ok());
        for input in [
            "",
            "ab",
            "root",
            "gh-123",
            " alice",
            "alice ",
            "alice/bob",
            "alice@example.com",
            "alice@github@github",
            "alice,",
            "álïce",
        ] {
            assert!(canonical_allow_principal(input).is_err(), "{input:?}");
        }
        assert!(canonical_allow_principal(&"a".repeat(33)).is_err());
        assert_ne!(
            canonical_allow_principal("alice").unwrap(),
            canonical_allow_principal("alice@github").unwrap()
        );
    }

    #[test]
    fn client_metadata_keeps_legacy_shape_and_validates_allow_policy() {
        let legacy = serde_json::json!({
            "name":"dev", "cwd":"/project", "hostname":"workstation"
        });
        let mut metadata: ClientMetadata = serde_json::from_value(legacy.clone()).unwrap();
        assert!(metadata.allow.is_empty());
        assert!(metadata.title.is_none());
        assert!(metadata.validate().is_ok());
        assert_eq!(serde_json::to_value(&metadata).unwrap(), legacy);
        metadata.allow = vec!["alice@github".into(), "bob@github".into()];
        assert!(metadata.validate().is_ok());
        assert_eq!(
            serde_json::to_value(&metadata).unwrap()["allow"],
            serde_json::json!(["alice@github", "bob@github"])
        );
        metadata.allow = vec!["alice".into(), "alice@github".into(), "bob_dev".into()];
        assert!(metadata.validate().is_ok());
        assert_eq!(
            serde_json::to_value(&metadata).unwrap()["allow"],
            serde_json::json!(["alice", "alice@github", "bob_dev"])
        );
        for allow in [
            vec!["Alice".into()],
            vec!["root".into()],
            vec!["alice".into(), "alice".into()],
            vec!["Alice@github".into()],
            vec!["alice@example.com".into()],
            vec!["alice@github".into(), "alice@github".into()],
            (0..=MAX_ALLOWED_PRINCIPALS)
                .map(|i| format!("user-{i}@github"))
                .collect(),
        ] {
            metadata.allow = allow;
            assert!(metadata.validate().is_err());
        }
        metadata.allow = (0..MAX_ALLOWED_PRINCIPALS)
            .map(|i| format!("user-{i}@github"))
            .collect();
        assert!(metadata.validate().is_ok());
    }

    #[test]
    fn session_titles_round_trip_and_are_validated_independently_of_client_names() {
        let legacy = serde_json::json!({
            "name":"dev", "cwd":"/project", "hostname":"workstation"
        });
        let mut metadata: ClientMetadata = serde_json::from_value(legacy).unwrap();
        metadata.title = Some("排查构建失败 · #42".into());
        assert!(metadata.validate().is_ok());
        let value = serde_json::to_value(&metadata).unwrap();
        assert_eq!(value["title"], "排查构建失败 · #42");
        assert_eq!(
            serde_json::from_value::<ClientMetadata>(value).unwrap(),
            metadata
        );
        for title in [
            "".into(),
            "   ".into(),
            "bad\ttitle".into(),
            "bad\0title".into(),
            "界".repeat(171),
        ] {
            metadata.title = Some(title);
            assert!(metadata.validate().is_err());
        }
        metadata.title = Some("界".repeat(170));
        assert!(metadata.validate().is_ok());
        metadata.title = Some("x".repeat(MAX_TITLE_BYTES));
        assert!(metadata.validate().is_ok());
        metadata.title = Some("x".repeat(MAX_TITLE_BYTES + 1));
        assert!(metadata.validate().is_err());
    }
    #[test]
    fn rejects_unknown_fields_and_invalid_limits() {
        assert!(
            serde_json::from_value::<Operation>(
                serde_json::json!({"op":"status","command":"ignored"})
            )
            .is_err()
        );
        assert!(
            Operation::Read {
                cursor: 0,
                max_bytes: MAX_READ + 1,
                wait_ms: 0
            }
            .validate()
            .is_err()
        );
        assert!(Operation::Resize { rows: 0, cols: 80 }.validate().is_err());
    }
}
