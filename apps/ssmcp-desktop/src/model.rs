//! Bounded display projections. Transport credentials never enter this module.
mod waits;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
pub use waits::WaitSnapshot;
use zgpui::bridge::{CommandOutcome, CommandRecord, protocol::*};

pub const HISTORY_ROWS: usize = 320;
pub const HISTORY_BYTES: usize = 256 * 1024;
pub const RECEIPTS: usize = 24;
const TERMINALS: usize = 16;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Named(String),
    Directory(std::path::PathBuf),
}

impl Target {
    pub fn label(&self) -> &str {
        match self {
            Self::Named(name) => name,
            Self::Directory(_) => "指定的本地会话",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum ThreadChoice {
    #[default]
    All,
    Main,
    Agent(String),
}

impl ThreadChoice {
    pub fn query(&self, cursor: Option<Counter>) -> Value {
        let mut value = json!({"limit":64});
        if let Some(cursor) = cursor {
            value["cursor"] = json!(cursor);
        }
        match self {
            Self::All => {}
            Self::Main => value["main"] = json!(true),
            Self::Agent(id) => value["thread_id"] = json!(id),
        }
        value
    }

    pub fn input_target(&self) -> &str {
        match self {
            Self::Agent(id) => id,
            Self::All | Self::Main => "next",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Session {
    pub name: String,
    pub connection: String,
    pub phase: String,
}

pub fn sessions(values: Vec<Value>) -> Vec<Session> {
    values
        .into_iter()
        .take(1024)
        .filter_map(|value| {
            let name = value["name"].as_str()?;
            if name.is_empty() || name.len() > 128 {
                return None;
            }
            Some(Session {
                name: name.to_owned(),
                connection: text(&value, "connection_state", 64),
                phase: text(&value, "phase", 64),
            })
        })
        .collect()
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Disconnected,
    Connecting,
    Observing,
    Controlling,
    Closing,
}

impl Phase {
    pub fn connected(self) -> bool {
        matches!(self, Self::Observing | Self::Controlling)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Disconnected => "未连接",
            Self::Connecting => "正在连接",
            Self::Observing => "观察中",
            Self::Controlling => "可控制",
            Self::Closing => "正在释放连接",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Row {
    pub sequence: Counter,
    pub kind: String,
    pub text: String,
    pub source: String,
    pub status: String,
    pub lossy: bool,
    pub message_id: Option<String>,
    pub input_side: bool,
    pub input_origin: Option<String>,
}

impl Row {
    fn bytes(&self) -> usize {
        self.kind.len()
            + self.text.len()
            + self.source.len()
            + self.status.len()
            + self.message_id.as_ref().map_or(0, String::len)
            + self.input_origin.as_ref().map_or(0, String::len)
            + 112
    }
}

#[derive(Clone, Debug, Default)]
pub struct History {
    pub rows: VecDeque<Row>,
    pub omitted: u64,
    bytes: usize,
}

impl History {
    pub fn push(&mut self, row: Row) {
        // The same page may be observed again after an interrupted state read.
        if self
            .rows
            .back()
            .is_some_and(|last| last.sequence >= row.sequence)
        {
            return;
        }
        self.bytes += row.bytes();
        self.rows.push_back(row);
        while self.rows.len() > HISTORY_ROWS || self.bytes > HISTORY_BYTES {
            if let Some(old) = self.rows.pop_front() {
                self.bytes -= old.bytes();
                self.omitted = self.omitted.saturating_add(1);
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct Agent {
    pub id: String,
    pub name: String,
    pub role: String,
    pub status: String,
}

#[derive(Clone, Debug)]
pub struct QueueItem {
    pub id: String,
    pub text: String,
    pub target: String,
    pub status: String,
    pub uncertain: bool,
    pub retarget_required: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Terminal {
    pub id: String,
    pub name: String,
    pub selected: bool,
    pub running: bool,
    pub rows: u16,
    pub cols: u16,
    pub screen: String,
    pub observed: bool,
    pub incomplete: bool,
    pub dimensions_confirmed: bool,
}

#[derive(Clone, Debug)]
pub struct Worker {
    pub slot: u64,
    pub epoch: String,
    pub state: String,
    pub registered: bool,
    pub task: String,
    pub progress: String,
    pub elapsed: String,
    pub retiring: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Workers {
    pub generation: String,
    pub total: u64,
    pub desired: u64,
    pub registered: u64,
    pub dreaming: Option<u64>,
    pub productivity: Option<[u64; 10]>,
    pub working: u64,
    pub idle: u64,
    pub offline: u64,
    pub retiring: u64,
    pub queued: u64,
    pub closed: bool,
    pub stopping: bool,
    pub slots: Vec<Worker>,
}

#[derive(Clone, Debug)]
pub struct Receipt {
    pub connection: u64,
    pub target: Option<String>,
    pub request_id: Id,
    pub generation: Id,
    pub command: Id,
    pub state: ReceiptState,
    pub detail: String,
    pub new_session: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptState {
    Sending,
    Confirmed,
    Rejected,
    Unknown,
}

impl ReceiptState {
    pub fn unresolved(self) -> bool {
        matches!(self, Self::Sending | Self::Unknown)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Sending => "等待回执",
            Self::Confirmed => "已收到回执",
            Self::Rejected => "未执行",
            Self::Unknown => "结果未知",
        }
    }
}

impl Receipt {
    pub fn from_record(record: &CommandRecord, connection: u64, arguments: &Value) -> Self {
        let target = match record.command.as_str() {
            "chat.send" => arguments
                .get("target")
                .and_then(Value::as_str)
                .or(Some("next")),
            "terminal.write" => arguments.get("terminal_id").and_then(Value::as_str),
            _ => None,
        }
        .map(str::to_owned);
        let (state, detail) = match &record.outcome {
            CommandOutcome::AwaitingReply => {
                (ReceiptState::Sending, "等待当前请求的真实回执。".into())
            }
            CommandOutcome::Error(error) if error.effect == Effect::MayHaveOccurred => (
                ReceiptState::Unknown,
                "请求可能已产生效果。请查看当前状态或原任务；客户端不会重发。".into(),
            ),
            CommandOutcome::Error(error) => {
                (ReceiptState::Rejected, error_label(error.code).into())
            }
            CommandOutcome::Receipt(receipt) => {
                let value = &receipt.value;
                let uncertain = matches!(value["status"].as_str(), Some("unknown" | "uncertain"))
                    || value
                        .get("uncertain")
                        .is_some_and(|value| value.as_bool() != Some(false));
                let chat_admitted = record.command.as_str() != "chat.send"
                    || (arguments["request_id"].as_str() == Some(record.request_id.as_str())
                        && value["request_id"].as_str() == Some(record.request_id.as_str())
                        && value["target"].as_str() == target.as_deref()
                        && value["confirmation"].as_str() == Some("local_queue_admission")
                        && value["message_id"]
                            .as_str()
                            .is_some_and(|value| Id::new(value).is_ok())
                        && matches!(
                            value["status"].as_str(),
                            Some("queued" | "sending" | "delivered")
                        ));
                let complete = match receipt.command.as_str() {
                    "session.new" => value["session"]
                        .as_str()
                        .is_some_and(|s| s.starts_with("live:") && s.len() > 5 && s.len() <= 256),
                    "session.cd" => value["cwd"].as_str().is_some_and(|s| !s.is_empty()),
                    _ => true,
                };
                if receipt.command != record.command || uncertain || !chat_admitted || !complete {
                    (
                        ReceiptState::Unknown,
                        "回执未确认原请求的操作结果。原请求已保留，请核对当前状态。".into(),
                    )
                } else {
                    let detail = match receipt.command.as_str() {
                        "chat.send" => format!(
                            "已确认原目标的本地输入队列接收：{}。投递状态见输入队列。",
                            text(value, "status", 64)
                        ),
                        "workers.resize" if value["resize_pending"] == true => {
                            "容量变更已接收，正在等待 worker 释放站位。".into()
                        }
                        "workers.resize" => "收到容量调整回执；当前站位见 Worker 页面。".into(),
                        "terminal.write" => "终端接受写入。命令是否完成请查看终端输出。".into(),
                        "session.cd" => text(value, "cwd", 2048),
                        "session.new" => "已新建会话".into(),
                        "terminal.resize" => "收到终端尺寸变更回执。".into(),
                        "client.update" => {
                            let summary = text(value, "summary", 2048);
                            let details = text(value, "details", 4096);
                            if details.is_empty() {
                                summary
                            } else {
                                format!("{summary}\n{details}")
                            }
                        }
                        _ => "收到原请求回执。完成语义由对应操作定义。".into(),
                    };
                    (ReceiptState::Confirmed, detail)
                }
            }
        };
        Self {
            connection,
            target,
            request_id: record.request_id.clone(),
            generation: record.generation.clone(),
            command: record.command.clone(),
            state,
            detail,
            new_session: match &record.outcome {
                CommandOutcome::Receipt(r)
                    if state == ReceiptState::Confirmed && r.command.as_str() == "session.new" =>
                {
                    r.value["session"].as_str().map(str::to_owned)
                }
                _ => None,
            },
        }
    }
}

#[derive(Clone, Default)]
pub struct View {
    pub connection: u64,
    pub sessions: Vec<Session>,
    pub cwd: String,
    pub launch_error: Option<String>,
    pub sessions_loading: bool,
    pub target: Option<Target>,
    pub phase: Phase,
    pub generation: Option<Id>,
    pub capabilities: Option<Capabilities>,
    pub notice: String,
    pub busy: bool,
    pub backend_active: bool,
    pub cursor: Option<Counter>,
    pub gaps: u64,
    pub timeline: History,
    pub conversation: History,
    pub conversation_more: bool,
    pub conversation_cursor: Option<Counter>,
    pub thread: ThreadChoice,
    pub agents: Vec<Agent>,
    pub threads_offset: usize,
    pub threads_next: Option<usize>,
    pub can_send: bool,
    pub reloading: bool,
    pub queue: Vec<QueueItem>,
    pub queue_more: bool,
    pub queue_offset: usize,
    pub queue_next: Option<usize>,
    pub terminals: Vec<Terminal>,
    pub workers: Option<Workers>,
    pub time: Option<WaitSnapshot>,
    pub receipts: VecDeque<Receipt>,
}

impl View {
    pub fn resource(&self, name: &str) -> bool {
        self.phase.connected()
            && self.capabilities.as_ref().is_some_and(|caps| {
                caps.resources
                    .iter()
                    .any(|resource| resource.id.as_str() == name && resource.schema_version == 1)
            })
    }

    pub fn command(&self, name: &str) -> bool {
        self.phase == Phase::Controlling
            && !self.busy
            && self.receipt_capacity()
            && self.capabilities.as_ref().is_some_and(|caps| {
                caps.mode == Mode::Control
                    && caps
                        .commands
                        .iter()
                        .any(|command| command.id.as_str() == name)
            })
    }

    pub fn receipt_capacity(&self) -> bool {
        self.receipts.len() < RECEIPTS
            || self
                .receipts
                .iter()
                .any(|receipt| !receipt.state.unresolved())
    }

    pub fn record(&mut self, receipt: Receipt) -> bool {
        if let Some(existing) = self.receipts.iter_mut().find(|item| {
            item.connection == receipt.connection
                && item.generation == receipt.generation
                && item.request_id == receipt.request_id
                && item.command == receipt.command
        }) {
            *existing = receipt;
            return true;
        }
        if self.receipts.len() >= RECEIPTS {
            let Some(index) = self
                .receipts
                .iter()
                .rposition(|item| !item.state.unresolved())
            else {
                return false;
            };
            self.receipts.remove(index);
        }
        self.receipts.push_front(receipt);
        true
    }

    pub fn clear_live(&mut self, clear_history: bool) {
        self.capabilities = None;
        self.generation = None;
        self.cursor = None;
        self.backend_active = false;
        self.cwd.clear();
        self.agents.clear();
        self.queue.clear();
        self.terminals.clear();
        self.workers = None;
        self.time = None;
        self.can_send = false;
        self.reloading = false;
        self.queue_more = false;
        if clear_history {
            self.timeline = History::default();
            self.conversation = History::default();
            self.conversation_cursor = None;
            self.conversation_more = false;
            self.gaps = 0;
        }
    }
}

pub fn bounded(value: &str, bytes: usize) -> String {
    let mut result = String::with_capacity(value.len().min(bytes));
    for ch in value.chars() {
        if ch.is_control() && ch != '\n' && ch != '\t' {
            continue;
        }
        if result.len() + ch.len_utf8() > bytes {
            result.push('…');
            break;
        }
        result.push(ch);
    }
    result
}

pub fn text(value: &Value, key: &str, bytes: usize) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .map_or_else(String::new, |value| bounded(value, bytes))
}

pub fn counter(value: &Value) -> Result<Counter, BridgeError> {
    serde_json::from_value(value.clone())
        .map_err(|_| BridgeError::invalid("invalid decimal counter"))
}

pub fn error_label(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::Denied => "当前连接没有这项权限，或租约已过期。",
        ErrorCode::Closed => "连接已经关闭，请重新连接。",
        ErrorCode::Unavailable => "暂时无法读取会话，请检查 CLI 是否在线后重新连接。",
        ErrorCode::StaleGeneration => "后端代次已改变，请重新连接以读取新的状态。",
        ErrorCode::InvalidCursor => "事件游标不再适用，请重新连接并刷新当前状态。",
        ErrorCode::TooLarge => "本次数据超过协议的大小限制。",
        ErrorCode::Conflict => "当前状态已经变化，请先刷新。",
        ErrorCode::UnsupportedVersion => "当前客户端与会话的协议版本不兼容。",
        ErrorCode::InvalidRequest => "数据未通过协议校验，本次结果没有用于更新界面。",
        ErrorCode::Internal => "会话报告内部错误，请检查 CLI 的本地诊断。",
    }
}

fn message_fields(value: &Value, projected: &Value) -> (Option<String>, bool, Option<String>) {
    if !matches!(
        value["type"].as_str(),
        Some("ssmcp.message" | "ssmcp.user_input")
    ) {
        return (None, false, None);
    }
    let id = projected["message_id"]
        .as_str()
        .or_else(|| value["message_id"].as_str())
        .filter(|id| ssmcp::client::replies::valid_message_id(id))
        .map(str::to_owned);
    let human = value["type"] == "ssmcp.user_input"
        || value["role"] == "user"
        || projected["input_origin"] == "human";
    let input = human || value["kind"] == "input" || projected["input_side"] == true;
    let origin = if human {
        Some("human".into())
    } else if input {
        Some(
            projected["input_origin"]
                .as_str()
                .or_else(|| value["input_origin"].as_str())
                .filter(|value| value.len() <= 32)
                .unwrap_or("system_hook")
                .into(),
        )
    } else {
        None
    };
    (id, input, origin)
}

pub fn event_row(event: &Event) -> Option<Row> {
    let frame = &event.value;
    if frame["kind"] == "terminal" {
        return None;
    }
    let value = frame.get("value").unwrap_or(frame);
    let kind = value["type"]
        .as_str()
        .or_else(|| frame["label"].as_str())
        .unwrap_or(event.topic.as_str());
    let body = [
        "text", "message", "summary", "stdout", "stderr", "output", "chunk",
    ]
    .into_iter()
    .filter_map(|key| value[key].as_str())
    .take(3)
    .collect::<Vec<_>>()
    .join("\n");
    let source = frame.get("source").unwrap_or(&Value::Null);
    let (message_id, input_side, input_origin) = message_fields(value, &Value::Null);
    let kind = if input_side {
        "input"
    } else if kind == "ssmcp.message" {
        if value["kind"] == "answer" {
            "answer"
        } else {
            "progress"
        }
    } else {
        kind
    };
    Some(Row {
        sequence: event.sequence,
        kind: bounded(kind, 128),
        text: bounded(if body.is_empty() { kind } else { &body }, 8192),
        source: source_label(source),
        status: text(value, "status", 64),
        lossy: frame["truncated"] == true,
        message_id,
        input_side,
        input_origin,
    })
}

fn source_label(source: &Value) -> String {
    if let Some(agent) = source["agent_id"].as_str() {
        bounded(agent, 128)
    } else if source["kind"] == "main" {
        "主会话".into()
    } else {
        "会话事件".into()
    }
}

/// Validate the complete history page before mutating any displayed rows/cursor.
pub fn conversation_page(
    value: &Value,
    previous: Option<Counter>,
) -> Result<(Vec<Row>, Counter, bool, bool), BridgeError> {
    let start = counter(&value["start_cursor"])?;
    let end = counter(&value["end_cursor"])?;
    let next = counter(&value["next_cursor"])?;
    let gap = value["gap"]
        .as_bool()
        .ok_or_else(|| BridgeError::invalid("missing history gap"))?;
    let more = value["has_more"]
        .as_bool()
        .ok_or_else(|| BridgeError::invalid("missing history continuation"))?;
    if start > next
        || next > end
        || previous.is_some_and(|previous| next < previous || (!gap && start > previous))
    {
        return Err(BridgeError::invalid("nonmonotonic history page"));
    }
    let values = value["events"]
        .as_array()
        .filter(|events| events.len() <= 64)
        .ok_or_else(|| BridgeError::invalid("invalid history events"))?;
    let mut rows = Vec::with_capacity(values.len());
    let mut last = None;
    for value in values {
        let sequence = counter(&value["sequence"])?;
        if sequence < start
            || sequence >= next
            || previous.is_some_and(|previous| sequence < previous)
            || last.is_some_and(|last| sequence <= last)
        {
            return Err(BridgeError::invalid("invalid history sequence"));
        }
        last = Some(sequence);
        // Keep operational envelopes in the Events page. Validate their
        // sequence first so filtering cannot hide a corrupt journal page.
        if !matches!(
            value["value"]["type"].as_str(),
            Some("ssmcp.message" | "ssmcp.user_input")
        ) && !matches!(
            value["kind"].as_str(),
            Some("input" | "answer" | "progress" | "message")
        ) {
            continue;
        }
        let (message_id, input_side, input_origin) = message_fields(&value["value"], value);
        rows.push(Row {
            sequence,
            kind: text(value, "kind", 128),
            text: text(value, "text", 8192),
            source: source_label(&value["source"]),
            status: text(value, "status", 64),
            lossy: value["text_lossy"] == true || value["truncated"] == true,
            message_id,
            input_side,
            input_origin,
        });
    }
    if more && (next >= end || previous == Some(next)) {
        return Err(BridgeError::invalid(
            "history continuation made no progress",
        ));
    }
    Ok((rows, next, more, gap))
}

/// Each terminal retains a bounded VT parser. A gap resets its screen so lost
/// escape sequences cannot silently leave a seemingly complete projection.
#[derive(Default)]
pub struct Screens {
    values: BTreeMap<String, vt100::Parser>,
    confirmed: BTreeMap<String, bool>,
    incomplete: BTreeSet<String>,
}

impl Screens {
    pub fn validate(event: &Event, terminals: &[Terminal]) -> Result<(), BridgeError> {
        if event.value["kind"] != "terminal"
            || !terminals
                .iter()
                .any(|terminal| event.value["terminal_id"] == terminal.id)
        {
            return Ok(());
        }
        let encoded = event.value["data_base64"]
            .as_str()
            .filter(|value| value.len() <= MAX_PAGE_BYTES)
            .ok_or_else(|| BridgeError::invalid("invalid terminal event"))?;
        STANDARD
            .decode(encoded)
            .map_err(|_| BridgeError::invalid("invalid terminal encoding"))?;
        Ok(())
    }

    pub fn reset(&mut self, terminals: &mut [Terminal]) {
        self.values.clear();
        for terminal in terminals {
            self.incomplete.insert(terminal.id.clone());
            terminal.screen.clear();
            terminal.observed = false;
            terminal.incomplete = true;
        }
    }

    pub fn sync(&mut self, value: &Value, previous: &[Terminal]) -> Vec<Terminal> {
        let Some(values) = value["terminals"].as_array() else {
            return Vec::new();
        };
        let mut result = Vec::new();
        for item in values.iter().take(TERMINALS) {
            let Some(id) = item["terminal_id"]
                .as_str()
                .filter(|id| !id.is_empty() && id.len() <= 128)
            else {
                continue;
            };
            let old = previous.iter().find(|terminal| terminal.id == id);
            // terminal.list does not advertise dimensions. Keep a known resize
            // receipt or an explicit local 24x80 projection default instead of
            // presenting these defaults as the actual PTY's dimensions.
            let existing = self.values.get(id).map(|parser| parser.screen().size());
            let rows = old
                .map(|terminal| terminal.rows)
                .or_else(|| existing.map(|size| size.0.get()))
                .unwrap_or(24);
            let cols = old
                .map(|terminal| terminal.cols)
                .or_else(|| existing.map(|size| size.1.get()))
                .unwrap_or(80);
            let parser = self
                .values
                .entry(id.to_owned())
                .or_insert_with(|| vt100::Parser::new(nonzero(rows), nonzero(cols), 0));
            parser.set_size(nonzero(rows), nonzero(cols));
            result.push(Terminal {
                id: id.to_owned(),
                name: text(item, "name", 128),
                selected: item["selected"] == true,
                running: item["running"] == true,
                rows,
                cols,
                screen: parser.screen().contents(),
                observed: old.is_some_and(|old| old.observed),
                incomplete: old.is_some_and(|old| old.incomplete) || self.incomplete.contains(id),
                dimensions_confirmed: self.confirmed.get(id).copied().unwrap_or(false),
            });
        }
        self.values
            .retain(|id, _| result.iter().any(|terminal| &terminal.id == id));
        self.confirmed
            .retain(|id, _| result.iter().any(|terminal| &terminal.id == id));
        self.incomplete
            .retain(|id| result.iter().any(|terminal| &terminal.id == id));
        result
    }

    pub fn resize(&mut self, terminals: &mut [Terminal], rows: u16, cols: u16) {
        for terminal in terminals {
            terminal.rows = rows.clamp(1, 200);
            terminal.cols = cols.clamp(1, 300);
            terminal.dimensions_confirmed = true;
            terminal.incomplete |= rows > 200 || cols > 300;
            if terminal.incomplete {
                self.incomplete.insert(terminal.id.clone());
            }
            self.confirmed.insert(terminal.id.clone(), true);
            if let Some(parser) = self.values.get_mut(&terminal.id) {
                parser.set_size(nonzero(terminal.rows), nonzero(terminal.cols));
                terminal.screen = parser.screen().contents();
            }
        }
    }

    pub fn apply(&mut self, event: &Event, terminals: &mut [Terminal]) -> Result<(), BridgeError> {
        if event.value["kind"] != "terminal" {
            return Ok(());
        }
        let Some(id) = event.value["terminal_id"].as_str() else {
            return Ok(());
        };
        let Some(terminal) = terminals.iter_mut().find(|terminal| terminal.id == id) else {
            return Ok(());
        };
        let encoded = event.value["data_base64"]
            .as_str()
            .filter(|value| value.len() <= MAX_PAGE_BYTES)
            .ok_or_else(|| BridgeError::invalid("invalid terminal event"))?;
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| BridgeError::invalid("invalid terminal encoding"))?;
        let parser = self.values.entry(id.to_owned()).or_insert_with(|| {
            vt100::Parser::new(nonzero(terminal.rows), nonzero(terminal.cols), 0)
        });
        parser.process(&bytes);
        terminal.screen = parser.screen().contents();
        terminal.observed = true;
        Ok(())
    }
}

fn nonzero(value: u16) -> std::num::NonZeroU16 {
    std::num::NonZeroU16::new(value.max(1)).expect("positive bounded terminal dimension")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_cursors_are_lossless_and_numeric_json_is_rejected() {
        assert_eq!(
            counter(&json!("9007199254740993")).unwrap(),
            Counter(9_007_199_254_740_993)
        );
        assert!(counter(&json!(9_007_199_254_740_993_u64)).is_err());
        assert!(counter(&json!("01")).is_err());
    }

    #[test]
    fn private_session_paths_and_errors_never_enter_the_projection() {
        let projected = sessions(vec![
            json!({"name":"work","connection_state":"connected","phase":"connected", "state_directory":"private-path", "last_error":"private-token"}),
        ]);
        let debug = format!("{projected:?}");
        assert!(!debug.contains("private"));
        assert_eq!(projected[0].name, "work");
    }

    #[test]
    fn malformed_later_history_item_rejects_the_whole_page() {
        let page = json!({"start_cursor":"1","end_cursor":"9","next_cursor":"8","gap":false,"has_more":true,
            "events":[{"sequence":"4","text":"first"},{"sequence":"4","text":"duplicate"}]});
        assert!(conversation_page(&page, Some(Counter(3))).is_err());
    }

    #[test]
    fn history_page_handles_filtered_noncontiguous_sequences() {
        let page = json!({"start_cursor":"1","end_cursor":"12","next_cursor":"10","gap":false,"has_more":true,
            "events":[{"sequence":"4","kind":"progress","text":"first"},{"sequence":"9","kind":"answer","text":"second"}]});
        let (rows, cursor, more, gap) = conversation_page(&page, Some(Counter(3))).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(cursor, Counter(10));
        assert!(more && !gap);
    }

    #[test]
    fn operational_tail_advances_cursor_without_displacing_conversation_messages() {
        let mut page = json!({
            "start_cursor":"1","end_cursor":"9","next_cursor":"9","gap":false,"has_more":false,
            "events":[
                {"sequence":"2","kind":"input","text":"human",
                 "value":{"type":"ssmcp.user_input","message_id":"human-original"}},
                {"sequence":"3","kind":"ssmcp.controller.finished","text":"finished",
                 "value":{"type":"ssmcp.controller.finished"}},
                {"sequence":"4","kind":"input","text":"timer",
                 "value":{"type":"ssmcp.message","kind":"input","input_origin":"agent_timer",
                          "message_id":"timer-original"}},
                {"sequence":"5","kind":"answer","text":"answer",
                 "value":{"type":"ssmcp.message","kind":"answer","message_id":"answer-original"}},
                {"sequence":"8","kind":"ssmcp.controller.started","text":"started",
                 "value":{"type":"ssmcp.controller.started"}}
            ]
        });
        let (rows, cursor, more, gap) = conversation_page(&page, Some(Counter(1))).unwrap();
        assert_eq!(cursor, Counter(9));
        assert!(!more && !gap);
        assert_eq!(
            rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
            ["human", "timer", "answer"]
        );
        assert!(rows[0].input_side && rows[1].input_side && !rows[2].input_side);
        assert_eq!(rows[1].message_id.as_deref(), Some("timer-original"));
        page["events"][4]["sequence"] = json!("3");
        assert!(conversation_page(&page, Some(Counter(1))).is_err());
    }

    #[test]
    fn retained_rows_are_bounded_and_duplicate_delivery_does_not_duplicate_text() {
        let mut history = History::default();
        for sequence in 0..1000 {
            history.push(Row {
                message_id: None,
                input_side: false,
                input_origin: None,
                sequence: Counter(sequence),
                kind: "event".into(),
                text: "中".repeat(2048),
                source: String::new(),
                status: String::new(),
                lossy: false,
            });
        }
        let count = history.rows.len();
        history.push(history.rows.back().unwrap().clone());
        assert_eq!(history.rows.len(), count);
        assert!(history.bytes <= HISTORY_BYTES && count <= HISTORY_ROWS && history.omitted > 0);
    }

    #[test]
    fn unknown_receipt_survives_disconnect_with_original_generation() {
        let record = CommandRecord {
            request_id: Id::new("request-1").unwrap(),
            generation: Id::new("old-generation").unwrap(),
            command: Id::new("chat.send").unwrap(),
            outcome: CommandOutcome::Error(
                BridgeError::new(ErrorCode::Unavailable, "private token").uncertain(),
            ),
        };
        let mut view = View::default();
        view.record(Receipt::from_record(
            &record,
            1,
            &json!({"request_id":"request-1","target":"next"}),
        ));
        view.clear_live(true);
        assert_eq!(view.receipts[0].state, ReceiptState::Unknown);
        assert_eq!(view.receipts[0].generation.as_str(), "old-generation");
        assert!(!view.receipts[0].detail.contains("private"));
    }

    #[test]
    fn terminal_gap_discards_partial_escape_state() {
        let mut screens = Screens::default();
        let mut terminals = screens.sync(
            &json!({"terminals":[{"terminal_id":"main","rows":24,"cols":80}]}),
            &[],
        );
        let event = Event {
            sequence: Counter(1),
            topic: Id::new("ssmcp.terminal").unwrap(),
            value: json!({"kind":"terminal","terminal_id":"main","data_base64":STANDARD.encode(b"hello\x1b[")}),
        };
        screens.apply(&event, &mut terminals).unwrap();
        assert!(terminals[0].screen.contains("hello"));
        screens.reset(&mut terminals);
        assert!(terminals[0].incomplete && terminals[0].screen.is_empty());
    }

    #[test]
    fn terminal_dimensions_are_defaults_until_an_actual_resize_receipt() {
        let mut screens = Screens::default();
        let state = json!({"terminals":[{"terminal_id":"main","running":true}]});
        let mut terminals = screens.sync(&state, &[]);
        assert!(!terminals[0].dimensions_confirmed);
        assert_eq!((terminals[0].rows, terminals[0].cols), (24, 80));
        screens.resize(&mut terminals, 32, 110);
        let terminals = screens.sync(&state, &terminals);
        assert!(terminals[0].dimensions_confirmed);
        assert_eq!((terminals[0].rows, terminals[0].cols), (32, 110));
    }
}

#[cfg(test)]
mod receipt_regressions {
    use super::*;

    fn project(record: &CommandRecord) -> Receipt {
        Receipt::from_record(
            record,
            1,
            &json!({"request_id":record.request_id,"target":"agent-one","terminal_id":"term-one"}),
        )
    }

    fn record(request_id: &str, command: &str, value: Value) -> CommandRecord {
        CommandRecord {
            request_id: Id::new(request_id).unwrap(),
            generation: Id::new("generation-one").unwrap(),
            command: Id::new(command).unwrap(),
            outcome: CommandOutcome::Receipt(CommandResult {
                command: Id::new(command).unwrap(),
                value,
            }),
        }
    }

    fn chat_value() -> Value {
        json!({"request_id":"send-one","target":"agent-one","confirmation":"local_queue_admission",
            "message_id":"message-one","status":"queued"})
    }

    #[test]
    fn chat_receipt_requires_inner_request_and_admission_proof() {
        for (key, value) in [
            ("request_id", json!("another-request")),
            ("confirmation", json!("model_read")),
            ("confirmation", Value::Null),
            ("target", json!("another-target")),
            ("message_id", json!("")),
            ("message_id", Value::Null),
        ] {
            let mut value_map = chat_value();
            value_map[key] = value;
            let receipt = project(&record("send-one", "chat.send", value_map));
            assert_eq!(receipt.state, ReceiptState::Unknown, "invalid {key}");
        }
    }

    #[test]
    fn native_unknown_and_uncertain_receipts_stay_unknown() {
        for value in [
            json!({"status":"unknown"}),
            json!({"status":"uncertain"}),
            json!({"status":"queued","uncertain":true}),
        ] {
            let receipt = project(&record("write-one", "terminal.write", value));
            assert_eq!(receipt.state, ReceiptState::Unknown);
        }
    }

    #[test]
    fn terminal_receipts_cannot_evict_an_unresolved_original() {
        let mut view = View::default();
        let mut unknown = record("original-unknown", "terminal.write", json!({}));
        unknown.outcome = CommandOutcome::Error(
            BridgeError::new(ErrorCode::Unavailable, "uncertain").uncertain(),
        );
        view.record(project(&unknown));
        for index in 0..RECEIPTS + 3 {
            view.record(project(&record(
                &format!("terminal-{index}"),
                "terminal.write",
                json!({}),
            )));
        }
        assert!(
            view.receipts
                .iter()
                .any(|receipt| receipt.request_id.as_str() == "original-unknown")
        );
        assert!(view.receipts.len() <= RECEIPTS);
    }

    #[test]
    fn all_unknown_capacity_blocks_further_command_admission() {
        let mut view = View {
            phase: Phase::Controlling,
            ..View::default()
        };
        view.capabilities = Some(Capabilities {
            application: Id::new("ssmcp").unwrap(),
            mode: Mode::Control,
            resources: vec![],
            commands: vec![Command {
                id: Id::new("terminal.write").unwrap(),
                description: "write".into(),
                input_schema: json!({}),
            }],
            events: false,
            max_frame_bytes: MAX_FRAME_BYTES as u32,
            max_event_page_bytes: MAX_PAGE_BYTES as u32,
        });
        for index in 0..RECEIPTS {
            let mut unknown = record(&format!("unknown-{index}"), "terminal.write", json!({}));
            unknown.outcome = CommandOutcome::Error(
                BridgeError::new(ErrorCode::Unavailable, "uncertain").uncertain(),
            );
            view.record(project(&unknown));
        }
        assert!(!view.command("terminal.write"));
    }
    #[test]
    fn exact_chat_admission_is_confirmed_for_supported_queue_states() {
        for status in ["queued", "sending", "delivered"] {
            let mut value = chat_value();
            value["status"] = json!(status);
            let receipt = project(&record("send-one", "chat.send", value));
            assert_eq!(receipt.state, ReceiptState::Confirmed);
            assert_eq!(receipt.target.as_deref(), Some("agent-one"));
        }
    }

    #[test]
    fn unresolved_capacity_rejects_new_records_but_accepts_original_completion() {
        let mut view = View::default();
        for index in 0..RECEIPTS {
            let mut item = record(&format!("original-{index}"), "terminal.write", json!({}));
            item.outcome = CommandOutcome::AwaitingReply;
            assert!(view.record(project(&item)));
        }
        assert!(!view.receipt_capacity());
        assert!(!view.record(project(&record("new-write", "terminal.write", json!({})))));
        assert_eq!(view.receipts.len(), RECEIPTS);
        assert!(
            view.receipts
                .iter()
                .all(|item| item.state == ReceiptState::Sending)
        );
        assert!(view.record(project(&record("original-0", "terminal.write", json!({})))));
        assert!(view.receipt_capacity());
        assert_eq!(view.receipts.len(), RECEIPTS);
    }

    #[test]
    fn another_connection_or_generation_cannot_replace_an_original_receipt() {
        let mut view = View::default();
        let mut original = record("same-id", "terminal.write", json!({}));
        original.outcome = CommandOutcome::Error(
            BridgeError::new(ErrorCode::Unavailable, "uncertain").uncertain(),
        );
        assert!(view.record(project(&original)));
        let mut other = project(&record("same-id", "terminal.write", json!({})));
        other.connection = 2;
        assert!(view.record(other));
        let mut another = project(&record("same-id", "terminal.write", json!({})));
        another.generation = Id::new("generation-two").unwrap();
        assert!(view.record(another));
        assert_eq!(view.receipts.len(), 3);
        assert!(view.receipts.iter().any(|receipt| receipt.connection == 1
            && receipt.generation.as_str() == "generation-one"
            && receipt.state == ReceiptState::Unknown));
    }
}
