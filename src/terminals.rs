//! Independent interactive terminals owned by one authenticated connection.
use crate::{backend::ztty, output::OutputStore, pty::Terminal as NativeTerminal, wire::Operation};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

const MAIN: &str = "main";
pub(crate) const MAX_TERMINALS: usize = 16;

enum SessionKind {
    Native(Arc<NativeTerminal>),
    Ztty(Arc<ztty::Terminal>),
}

/// One interactive session. The selected host owns its PTY and output lifecycle.
pub struct Session {
    inner: SessionKind,
}
impl Session {
    fn native(terminal: Arc<NativeTerminal>) -> Arc<Self> {
        Arc::new(Self {
            inner: SessionKind::Native(terminal),
        })
    }
    fn ztty(terminal: Arc<ztty::Terminal>) -> Arc<Self> {
        Arc::new(Self {
            inner: SessionKind::Ztty(terminal),
        })
    }
    pub async fn perform(&self, operation: Operation) -> Result<Value, String> {
        match &self.inner {
            SessionKind::Native(terminal) => terminal.perform(operation).await,
            SessionKind::Ztty(terminal) => terminal.perform(operation).await,
        }
    }
    pub async fn shutdown(&self) {
        match &self.inner {
            SessionKind::Native(terminal) => terminal.shutdown().await,
            SessionKind::Ztty(terminal) => terminal.shutdown().await,
        }
    }
}

#[derive(Clone)]
struct Entry {
    terminal: Arc<Session>,
    name: String,
    cwd: PathBuf,
}

struct State {
    entries: BTreeMap<String, Entry>,
    selected: String,
    next: u64,
    closed: bool,
}

pub struct Terminals {
    root: PathBuf,
    shell: PathBuf,
    output: Arc<OutputStore>,
    state: Mutex<State>,
    ztty: Option<Arc<ztty::Host>>,
    opening: tokio::sync::Mutex<()>,
}

impl Terminals {
    pub fn new(shell: &Path, root: PathBuf, output: Arc<OutputStore>) -> crate::Result<Arc<Self>> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err("terminal workspace must be a directory".into());
        }
        crate::client::terminal::register_session(MAIN, MAIN);
        let terminal = Session::native(NativeTerminal::spawn_named_with_store(
            shell,
            &root,
            output.clone(),
            MAIN,
        )?);
        let main = Entry {
            terminal,
            name: MAIN.into(),
            cwd: root.clone(),
        };
        Ok(Arc::new(Self {
            root,
            shell: shell.into(),
            output,
            ztty: None,
            opening: tokio::sync::Mutex::new(()),
            state: Mutex::new(State {
                entries: BTreeMap::from([(MAIN.into(), main)]),
                selected: MAIN.into(),
                next: 1,
                closed: false,
            }),
        }))
    }

    pub(crate) async fn new_ztty(
        program: &Path,
        root: PathBuf,
        output: Arc<OutputStore>,
    ) -> crate::Result<Arc<Self>> {
        let root = root.canonicalize()?;
        if !root.is_dir() {
            return Err("terminal workspace must be a directory".into());
        }
        let host = ztty::Host::start(program, &root, output.clone()).await?;
        crate::client::terminal::register_session(MAIN, MAIN);
        let terminal = match host.open(MAIN.into(), root.clone()).await {
            Ok(terminal) => Session::ztty(terminal),
            Err(error) => {
                host.shutdown().await;
                crate::client::terminal::close_session(MAIN);
                return Err(error.into());
            }
        };
        let main = Entry {
            terminal,
            name: MAIN.into(),
            cwd: root.clone(),
        };
        Ok(Arc::new(Self {
            root,
            shell: crate::platform::default_shell(),
            output,
            ztty: Some(host),
            opening: tokio::sync::Mutex::new(()),
            state: Mutex::new(State {
                entries: BTreeMap::from([(MAIN.into(), main)]),
                selected: MAIN.into(),
                next: 1,
                closed: false,
            }),
        }))
    }

    pub(crate) fn ztty(&self) -> Option<Arc<ztty::Host>> {
        self.ztty.clone()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn get(&self, id: &str) -> Result<Arc<Session>, String> {
        self.state()
            .entries
            .get(id)
            .map(|entry| entry.terminal.clone())
            .ok_or_else(|| "unknown or closed terminal_id; use terminal.list".into())
    }

    pub fn primary(&self) -> Arc<Session> {
        self.get(MAIN).expect("main terminal exists until shutdown")
    }

    pub fn selected(&self) -> Arc<Session> {
        let state = self.state();
        state
            .entries
            .get(&state.selected)
            .expect("selected terminal exists")
            .terminal
            .clone()
    }

    pub fn cycle(&self) -> Result<(), String> {
        let mut state = self.state();
        if state.closed {
            return Err("terminal manager is closed".into());
        }
        let ids: Vec<_> = state.entries.keys().cloned().collect();
        let current = ids.iter().position(|id| id == &state.selected).unwrap_or(0);
        state.selected = ids[(current + 1) % ids.len()].clone();
        crate::client::terminal::select_session(&state.selected);
        Ok(())
    }

    pub async fn resize_all(&self, rows: u16, cols: u16) -> Result<(), String> {
        // A desktop ztty view owns its geometry; dashboard layout is independent.
        // Explicit terminal.resize and ztty.session_resize still resize that PTY.
        if self.ztty.is_some() {
            return Ok(());
        }
        let entries: Vec<_> = self.state().entries.values().cloned().collect();
        for entry in entries {
            let status = entry.terminal.perform(Operation::Status {}).await?;
            if status["running"] == true
                && let Err(error) = entry
                    .terminal
                    .perform(Operation::Resize { rows, cols })
                    .await
                && entry.terminal.perform(Operation::Status {}).await?["running"] == true
            {
                return Err(error);
            }
        }
        Ok(())
    }

    async fn open(&self, args: Value) -> Result<(String, Entry), String> {
        let _opening = self.opening.lock().await;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Open {
            name: Option<String>,
            cwd: Option<PathBuf>,
            #[serde(default)]
            select: bool,
        }
        let args: Open = serde_json::from_value(args).map_err(|error| error.to_string())?;
        if args.name.as_ref().is_some_and(|name| {
            name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control)
        }) {
            return Err(
                "terminal name must contain 1..=128 UTF-8 bytes and no control characters".into(),
            );
        }
        let requested = args.cwd.unwrap_or_else(|| self.root.clone());
        let cwd = if requested.is_absolute() {
            requested
        } else {
            self.root.join(requested)
        }
        .canonicalize()
        .map_err(|error| format!("invalid terminal cwd: {error}"))?;
        if !cwd.is_dir() || !cwd.starts_with(&self.root) {
            return Err(
                "terminal cwd must be an existing directory inside the authorized workspace".into(),
            );
        }
        let id = {
            let mut state = self.state();
            if state.closed {
                return Err("terminal manager is closed".into());
            }
            if state.entries.len() >= MAX_TERMINALS {
                return Err(format!(
                    "at most {MAX_TERMINALS} interactive terminals may be open; close an existing terminal"
                ));
            }
            let id = format!("pty-{:04}", state.next);
            state.next += 1;
            id
        };
        let name = args.name.unwrap_or_else(|| id.clone());
        crate::client::terminal::register_session(&id, &name);
        let created = if let Some(host) = &self.ztty {
            host.open(id.clone(), cwd.clone()).await.map(Session::ztty)
        } else {
            NativeTerminal::spawn_named_with_store(&self.shell, &cwd, self.output.clone(), &id)
                .map(Session::native)
                .map_err(|error| error.to_string())
        };
        let terminal = match created {
            Ok(terminal) => terminal,
            Err(error) => {
                crate::client::terminal::close_session(&id);
                return Err(error);
            }
        };
        let entry = Entry {
            terminal,
            name,
            cwd,
        };
        let accepted = {
            let mut state = self.state();
            if state.closed {
                false
            } else {
                state.entries.insert(id.clone(), entry.clone());
                if args.select {
                    state.selected = id.clone();
                    crate::client::terminal::select_session(&id);
                }
                true
            }
        };
        if !accepted {
            entry.terminal.shutdown().await;
            crate::client::terminal::close_session(&id);
            return Err("terminal manager closed while opening the session".into());
        }
        Ok((id, entry))
    }

    async fn snapshot(&self, id: &str, entry: &Entry) -> Result<Value, String> {
        let mut result = entry.terminal.perform(Operation::Status {}).await?;
        result["terminal_id"] = json!(id);
        result["name"] = json!(entry.name);
        result["cwd"] = json!(entry.cwd);
        result["cwd_kind"] = json!("startup");
        result["selected"] = json!(self.state().selected == id);
        Ok(result)
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "terminal.open" => {
                let (id, entry) = self.open(args).await?;
                self.snapshot(&id, &entry).await
            }
            "terminal.list" => {
                if !args.as_object().is_some_and(|args| args.is_empty()) {
                    return Err("terminal.list takes no arguments".into());
                }
                let (entries, selected) = {
                    let state = self.state();
                    (state.entries.clone(), state.selected.clone())
                };
                let mut terminals = Vec::with_capacity(entries.len());
                for (id, entry) in entries {
                    terminals.push(self.snapshot(&id, &entry).await?);
                }
                Ok(
                    json!({"terminals":terminals,"selected":selected,"default_terminal_id":MAIN,"max_terminals":MAX_TERMINALS}),
                )
            }
            "terminal.select" => {
                let id = required_id(args)?;
                let entry = {
                    let mut state = self.state();
                    let entry = state
                        .entries
                        .get(&id)
                        .cloned()
                        .ok_or("unknown or closed terminal_id; use terminal.list")?;
                    state.selected = id.clone();
                    crate::client::terminal::select_session(&id);
                    entry
                };
                self.snapshot(&id, &entry).await
            }
            "terminal.close" => {
                let id = required_id(args)?;
                if id == MAIN {
                    return Err(
                        "main is the connection terminal; disconnect the client to close it".into(),
                    );
                }
                let entry = {
                    let mut state = self.state();
                    let entry = state.entries.remove(&id);
                    if state.selected == id {
                        state.selected = MAIN.into();
                        crate::client::terminal::select_session(MAIN);
                    }
                    crate::client::terminal::close_session(&id);
                    entry
                };
                let closed = entry.is_some();
                if let Some(entry) = entry {
                    entry.terminal.shutdown().await;
                }
                Ok(json!({"terminal_id":id,"closed":closed,"selected":self.state().selected}))
            }
            _ => {
                let mut args = args
                    .as_object()
                    .cloned()
                    .ok_or("terminal arguments must be an object")?;
                let id = match args.remove("terminal_id") {
                    None => MAIN.to_owned(),
                    Some(Value::String(id)) if valid_id(&id) => id,
                    _ => {
                        return Err(
                            "terminal_id must be a nonempty string of at most 128 bytes".into()
                        );
                    }
                };
                let terminal = self.get(&id)?;
                let operation = crate::backend::terminal_operation(name, Value::Object(args))?;
                let mut result = terminal.perform(operation).await?;
                result["terminal_id"] = json!(id);
                Ok(result)
            }
        }
    }

    pub async fn shutdown(&self) {
        let entries = {
            let mut state = self.state();
            state.closed = true;
            std::mem::take(&mut state.entries)
        };
        futures_util::future::join_all(entries.into_iter().map(|(id, entry)| async move {
            entry.terminal.shutdown().await;
            crate::client::terminal::close_session(&id);
        }))
        .await;
        if let Some(host) = &self.ztty {
            host.shutdown().await;
        }
    }

    pub fn management_tools() -> Vec<Value> {
        let id = json!({"terminal_id":{"type":"string","minLength":1,"maxLength":128}});
        vec![
            tool(
                "terminal.open",
                "Create an independent interactive shell with its own process, cwd, input, output cursor and virtual terminal state. Access it through terminal.read/write or an interactive frontend. Continues while unselected; ends when closed or the client disconnects.",
                json!({"name":{"type":"string","minLength":1,"maxLength":128},
                    "cwd":{"type":"string","description":"Existing directory inside this client's workspace; defaults to the workspace root."},
                    "select":{"type":"boolean","default":false,"description":"Mark this terminal as selected for interactive frontends. The built-in dashboard remains an audit interface and keeps keyboard input for navigation."}}),
                &[],
                false,
            ),
            tool(
                "terminal.list",
                "List independent terminal sessions and the selected terminal's identity for interactive frontends. The audit dashboard does not display their shell output. Calls without terminal_id always target main.",
                json!({}),
                &[],
                true,
            ),
            tool(
                "terminal.select",
                "Mark one terminal as selected for interactive frontends. The audit dashboard keeps keyboard input for navigation; use terminal.write with an explicit terminal_id for shell input. Other terminals keep running and retain independent output.",
                id.clone(),
                &["terminal_id"],
                false,
            ),
            tool(
                "terminal.close",
                "Stop and release one additional terminal and its owned shell processes. Repeated closes are harmless. The main terminal ends with the client connection.",
                id,
                &["terminal_id"],
                false,
            ),
        ]
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control)
}
fn required_id(args: Value) -> Result<String, String> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Id {
        terminal_id: String,
    }
    let args: Id = serde_json::from_value(args).map_err(|error| error.to_string())?;
    if !valid_id(&args.terminal_id) {
        return Err("terminal_id must be a nonempty string of at most 128 bytes".into());
    }
    Ok(args.terminal_id)
}
fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    read_only: bool,
) -> Value {
    json!({"name":name,"description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":read_only,"destructiveHint":!read_only,"openWorldHint":!read_only}})
}
