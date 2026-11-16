//! Claude Code's native stdio MCP server, with no copied tool catalogue.
//!
//! `claude mcp serve` exposes the installed version's tools (including Agent on
//! current versions), not the interactive Claude terminal. Tool calls and native
//! progress are logged by Peer. Native foreground Bash output is returned when
//! the call finishes; it is not a byte stream. Background Bash calls on Claude
//! Code >= 2.1.295 return an output path, which we follow incrementally. Claude
//! does not send a background-completion notification, so watchers live until
//! disconnect or file removal. A fixed limit bounds concurrent watchers.
use super::rpc::{Dialect, Peer};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    ffi::{OsStr, OsString},
    io,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    fs::File,
    io::{AsyncReadExt, AsyncSeekExt},
    process::Command,
    task::JoinHandle,
};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(windows)]
use std::os::windows::fs::MetadataExt;

const MAX_TOOL_PAGES: usize = 256;
const MAX_BACKGROUND_WATCHERS: usize = 32;
const OUTPUT_CHUNK: usize = 8192;

pub struct Claude {
    peer: Arc<Peer>,
    // A synchronous mutex keeps Drop able to abort every watcher. Never held
    // across an await, and the collection is small and bounded.
    watchers: Mutex<HashMap<PathBuf, JoinHandle<()>>>,
    closed: AtomicBool,
    next_progress: AtomicU64,
}

impl Claude {
    pub async fn start(program: &OsStr, args: &[OsString]) -> crate::Result<Arc<Self>> {
        Self::start_with_output(program, args, crate::output::OutputStore::global()?).await
    }

    pub async fn start_with_output(
        program: &OsStr,
        args: &[OsString],
        output: Arc<crate::output::OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        validate_args(args)?;
        let mut command = Command::new(program);
        command.args(["mcp", "serve"]).args(args);
        let peer = Peer::spawn_with_output(command, "claude", Dialect::JsonRpc, output).await?;
        let initialization = async {
            let initialized = peer
                .request(
                    "initialize",
                    json!({
                        "protocolVersion":"2025-11-25",
                        // No sampling or automatic approval capabilities.
                        "capabilities":{},
                        "clientInfo":{"name":"ssmcp","version":env!("CARGO_PKG_VERSION")}
                    }),
                )
                .await?;
            if !initialized["protocolVersion"].is_string()
                || !initialized["capabilities"]["tools"].is_object()
            {
                return Err("Claude Code did not initialize an MCP tools server".to_owned());
            }
            peer.notify("notifications/initialized", json!({})).await
        }
        .await;
        if let Err(error) = initialization {
            peer.shutdown().await;
            return Err(error.into());
        }
        Ok(Arc::new(Self {
            peer,
            watchers: Mutex::new(HashMap::new()),
            closed: AtomicBool::new(false),
            next_progress: AtomicU64::new(1),
        }))
    }

    pub async fn tools(&self) -> Result<Vec<Value>, String> {
        // Always discover again. This also honours notifications/tools/list_changed
        // without a second catalogue cache that could become stale.
        let mut tools = control_tools();
        let mut names = HashSet::new();
        let mut cursors = HashSet::new();
        let mut params = json!({});
        for _ in 0..MAX_TOOL_PAGES {
            let page = self.peer.request("tools/list", params).await?;
            let entries = page["tools"]
                .as_array()
                .ok_or("Claude Code tools/list returned no tools array")?;
            for entry in entries {
                let name = entry["name"]
                    .as_str()
                    .filter(|name| !name.is_empty())
                    .ok_or("Claude Code returned a tool without a name")?;
                if !names.insert(name.to_owned()) {
                    return Err(format!("Claude Code returned duplicate tool name {name:?}"));
                }
                let mut entry = entry.clone();
                // Keep schemas, annotations, extension fields, and descriptions
                // intact; only the namespace on the public tool name changes.
                entry["name"] = Value::String(format!("cc.{name}"));
                tools.push(entry);
            }
            match page.get("nextCursor") {
                None | Some(Value::Null) => return Ok(tools),
                Some(Value::String(cursor)) if !cursor.is_empty() => {
                    if !cursors.insert(cursor.clone()) {
                        return Err("Claude Code repeated a tools/list cursor".into());
                    }
                    params = json!({"cursor":cursor});
                }
                _ => return Err("Claude Code returned an invalid tools/list cursor".into()),
            }
        }
        Err("Claude Code exceeded the tools/list pagination limit".into())
    }

    pub async fn call(&self, name: &str, arguments: Value) -> Result<Value, String> {
        match name {
            "ssmcp.cc.events" => Ok(super::result(self.peer.events().read(arguments).await?)),
            "ssmcp.cc.requests" => Ok(super::result(self.peer.pending().await)),
            "ssmcp.cc.respond" => Ok(super::result(self.peer.respond(arguments).await?)),
            _ => {
                let native = name
                    .strip_prefix("cc.")
                    .filter(|name| !name.is_empty())
                    .ok_or("unknown Claude Code tool")?;
                let result = self
                    .peer
                    .request("tools/call", json!({
                        "name":native,"arguments":arguments,
                        "_meta":{"progressToken":format!("ssmcp-cc-{}",self.next_progress.fetch_add(1,Ordering::Relaxed))}
                    }))
                    .await?;
                if native == "Bash" && result.get("isError") != Some(&Value::Bool(true)) {
                    self.watch_background(&result);
                }
                // This is already a CallToolResult. Do not wrap it in text or
                // discard images, structuredContent, errors, or future fields.
                Ok(result)
            }
        }
    }

    fn watch_background(&self, result: &Value) {
        for path in background_paths(result) {
            let mut watchers = self.watchers.lock().unwrap_or_else(|e| e.into_inner());
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            watchers.retain(|_, task| !task.is_finished());
            if watchers.contains_key(&path) {
                continue;
            }
            if watchers.len() >= MAX_BACKGROUND_WATCHERS {
                let event = json!({
                    "type":"background_output_error","backend":"cc","path":path,
                    "error":"background output watcher limit reached; use cc.Read to read this file"
                });
                self.peer.events().push(event);
                continue;
            }
            let peer = self.peer.clone();
            let watched_path = path.clone();
            let task = tokio::spawn(async move {
                if let Err(error) = tail_background(&peer, &watched_path).await {
                    let event = json!({
                        "type":"background_output_error","backend":"cc",
                        "path":watched_path,"error":error.to_string()
                    });
                    peer.events().push(event);
                }
            });
            watchers.insert(path, task);
        }
    }

    pub async fn shutdown(&self) {
        for task in self.stop_watchers() {
            let _ = task.await;
        }
        self.peer.shutdown().await;
    }

    fn stop_watchers(&self) -> Vec<JoinHandle<()>> {
        self.closed.store(true, Ordering::Release);
        let mut watchers = self.watchers.lock().unwrap_or_else(|e| e.into_inner());
        let tasks: Vec<_> = watchers.drain().map(|(_, task)| task).collect();
        for task in &tasks {
            task.abort();
        }
        tasks
    }
}

fn validate_args(args: &[OsString]) -> Result<(), String> {
    for argument in args {
        let argument = argument.to_string_lossy();
        let flag = argument
            .split_once('=')
            .map_or(argument.as_ref(), |(flag, _)| flag);
        if matches!(
            flag,
            "-p" | "--print" | "--input-format" | "--output-format" | "--sdk-url"
        ) {
            return Err(format!(
                "{flag} is reserved for the Claude transport; cc mode requires native `claude mcp serve` stdio"
            ));
        }
    }
    Ok(())
}

impl Drop for Claude {
    fn drop(&mut self) {
        drop(self.stop_watchers());
    }
}

fn control_tools() -> Vec<Value> {
    vec![
        json!({
            "name":"ssmcp.cc.events",
            "description":"Read Claude Code events, requests, responses, native progress, and incremental background Bash output. Save next_cursor for the next call; a bounded buffer may report truncated=true.",
            "inputSchema":{"type":"object","properties":{
                "cursor":{"type":"integer","minimum":0,"default":0},
                "max_events":{"type":"integer","minimum":1,"maximum":256,"default":50},
                "wait_ms":{"type":"integer","minimum":0,"maximum":5000,"default":0}
            },"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        }),
        json!({
            "name":"ssmcp.cc.requests",
            "description":"List native server requests waiting for a response. Requests are never automatically approved; review one before responding with ssmcp.cc.respond.",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        }),
        json!({
            "name":"ssmcp.cc.respond",
            "description":"Respond to one pending native server request by its exact id with either result or a JSON-RPC error. Supply exactly one of result and error.",
            "inputSchema":{"type":"object","properties":{
                "id":{"type":["string","integer"]},
                "result":{},
                "error":{"type":"object","properties":{
                    "code":{"type":"integer"},"message":{"type":"string"},"data":{}
                },"required":["code","message"]}
            },"required":["id"],"additionalProperties":false}
        }),
    ]
}

fn background_paths(result: &Value) -> Vec<PathBuf> {
    let mut paths = HashSet::new();
    let mut add = |payload: &Value| {
        if let Some(path) = payload["backgroundOutputPath"].as_str() {
            let path = PathBuf::from(path);
            if path.is_absolute() {
                paths.insert(path);
            }
        }
    };
    // Current Claude Code wraps its native result in a JSON text block. Accept
    // the standard structured form too, without guessing any output directory.
    if let Some(payload) = result.get("structuredContent") {
        add(payload);
    }
    if let Some(content) = result["content"].as_array() {
        for block in content {
            if block["type"] == "text"
                && let Some(text) = block["text"].as_str()
                && let Ok(payload) = serde_json::from_str::<Value>(text)
            {
                add(&payload);
            }
        }
    }
    paths.into_iter().collect()
}

async fn open_background(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "output path is not absolute",
        ));
    }
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    let file = options.open(path).await?;
    let metadata = file.metadata().await?;
    let regular = metadata.is_file();
    #[cfg(unix)]
    let regular =
        regular && metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0;
    #[cfg(windows)]
    let regular = regular
        && metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
            == 0;
    if !regular {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "output must be a regular file owned by this user without unsafe links or permissions",
        ));
    }
    Ok(file)
}

async fn tail_background(peer: &Peer, path: &Path) -> io::Result<()> {
    let mut file = open_background(path).await?;
    let mut offset = 0u64;
    let mut bytes = [0u8; OUTPUT_CHUNK];
    loop {
        let count = file.read(&mut bytes).await?;
        if count > 0 {
            let chunk = &bytes[..count];
            let event = json!({
                "type":"background_output","backend":"cc","path":path,"offset":offset,
                "text":String::from_utf8_lossy(chunk),"data_base64":STANDARD.encode(chunk)
            });
            // JSON escaping keeps control bytes from changing the observer's
            // terminal. Each newly appended byte is recorded only once.
            peer.events().push(event);
            offset += count as u64;
            tokio::task::yield_now().await;
            continue;
        }
        let metadata = file.metadata().await?;
        #[cfg(unix)]
        if metadata.nlink() == 0 {
            return Ok(());
        }
        #[cfg(windows)]
        if !path.try_exists()? {
            return Ok(());
        }
        if metadata.len() < offset {
            file.seek(io::SeekFrom::Start(0)).await?;
            offset = 0;
            peer.events().push(json!({
                "type":"background_output_reset","backend":"cc","path":path
            }));
        }
        // No completion event exists in the native MCP protocol. Do not invent
        // one from a quiet file or scan arbitrary Claude state directories.
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    #[test]
    fn background_path_must_be_an_explicit_top_level_native_result_field() {
        assert_eq!(
            background_paths(&json!({"content":[{"type":"text","text":
                "{\"backgroundOutputPath\":\"/tmp/ssmcp-test-output\"}"}]})),
            [PathBuf::from("/tmp/ssmcp-test-output")]
        );
        assert!(
            background_paths(&json!({"content":[
                {"type":"text","text":"backgroundOutputPath: /etc/passwd"},
                {"type":"text","text":"{\"stdout\":{\"backgroundOutputPath\":\"/etc/passwd\"}}"},
                {"type":"text","text":"{\"backgroundOutputPath\":\"relative-output\"}"}
            ]}))
            .is_empty()
        );
    }

    #[tokio::test]
    async fn output_watcher_rejects_symlinks_directories_and_shared_writable_files() {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("ssmcp-cc-watch-{}-{unique}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let file = directory.join("output");
        let link = directory.join("link");
        std::fs::write(&file, b"test output").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
        symlink(&file, &link).unwrap();
        assert!(open_background(&file).await.is_ok());
        assert!(open_background(&link).await.is_err());
        assert!(open_background(&directory).await.is_err());
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert!(open_background(&file).await.is_err());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
