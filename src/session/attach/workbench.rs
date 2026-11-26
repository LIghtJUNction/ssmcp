//! Dispatch TUI session commands through the same zbridge host as GUI clients.
use super::*;
use zbridge::{Id, Operation, Request, Response, ResponseBody};

#[derive(Clone)]
struct Attached {
    directory: PathBuf,
    lease: String,
    read_only: bool,
    handle: tokio::runtime::Handle,
    input_generation: Arc<tokio::sync::OnceCell<(u64, Id)>>,
}

static ATTACHED: Mutex<Option<Attached>> = Mutex::new(None);

pub(super) struct Binding;
impl Binding {
    pub(super) fn open(directory: &Path, lease: &str, read_only: bool) -> Self {
        *ATTACHED.lock().unwrap_or_else(|e| e.into_inner()) = Some(Attached {
            directory: directory.into(),
            lease: lease.into(),
            read_only,
            handle: tokio::runtime::Handle::current(),
            input_generation: Arc::new(tokio::sync::OnceCell::new()),
        });
        Self
    }
}
impl Drop for Binding {
    fn drop(&mut self) {
        ATTACHED.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}

enum Route {
    Embedded(Frontend),
    Attached(Attached),
}

impl Route {
    async fn request(&self, operation: Operation) -> crate::Result<Response> {
        let request = Request {
            version: zbridge::VERSION,
            id: Id::new(super::super::random_suffix()?)?,
            operation,
        };
        let response = match self {
            Self::Embedded(frontend) => frontend.request(request.clone()).await,
            Self::Attached(attached) => {
                let value = ipc::request(
                    &attached.directory,
                    ENDPOINT,
                    json!({"op":"bridge","lease":attached.lease,"request":&request}),
                )
                .await?;
                serde_json::from_value(value)?
            }
        };
        response.validate_for(&request)?;
        if let ResponseBody::Error(error) = response.body {
            return Err(error.into());
        }
        Ok(response)
    }

    async fn perform(&self, command: &'static str, arguments: Value) -> crate::Result<Value> {
        let capabilities = self.request(Operation::Capabilities {}).await?;
        let generation = capabilities.generation.ok_or("Session is connecting")?;
        let response = self
            .request(Operation::Command {
                generation,
                command: Id::new(command)?,
                arguments,
            })
            .await?;
        let ResponseBody::Command(result) = response.body else {
            return Err("Missing command receipt".into());
        };
        Ok(result.value)
    }

    fn current() -> crate::Result<Self> {
        if let Some(attached) = ATTACHED.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            if attached.read_only {
                return Err("This session is read-only".into());
            }
            return Ok(Self::Attached(attached));
        }
        let capture = capture().ok_or("This session has no local frontend")?;
        Ok(Self::Embedded(Frontend::open(&capture, false)?))
    }
}

pub(crate) fn chat_bound() -> bool {
    ATTACHED.lock().unwrap_or_else(|e| e.into_inner()).is_some()
        || capture().is_some_and(|c| {
            c.runtime
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .runtime
                .as_ref()
                .is_some_and(|r| r.frontend_client_id().is_some())
        })
}

pub(crate) enum ChatSubmission {
    Local(String),
    Attached(crate::client::chat_io::Ticket),
}

/// An embedded chat admission is synchronous inside zbridge. A separate attach
/// process waits asynchronously and keeps its draft until the native receipt.
pub(crate) fn submit_chat(text: &str, target: &str) -> crate::Result<ChatSubmission> {
    use futures_util::FutureExt;
    let args = json!({"request_id":super::super::random_suffix()?,"text":text,"target":target});
    let route = Route::current()?;
    if let Route::Attached(attached) = &route {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let handle = attached.handle.clone();
        handle.spawn(async move {
            let value = route
                .perform("chat.send", args)
                .await
                .map_err(|e| e.to_string())
                .and_then(|v| {
                    let id = v["message_id"]
                        .as_str()
                        .ok_or("Missing chat admission")?
                        .to_owned();
                    Ok(crate::client::chat_io::Receipt {
                        status: "queued".into(),
                        request_id: Some(id),
                        warnings: vec![],
                        output: None,
                        display: None,
                    })
                });
            let _ = tx.send(value);
        });
        Ok(ChatSubmission::Attached(rx))
    } else {
        let receipt = route
            .perform("chat.send", args)
            .now_or_never()
            .ok_or("Chat admission was interrupted; inspect the queue before retrying")??;
        Ok(ChatSubmission::Local(
            receipt["message_id"]
                .as_str()
                .ok_or("Missing chat admission")?
                .into(),
        ))
    }
}

pub(crate) async fn terminal_write(terminal: &str, text: &str) -> crate::Result<()> {
    Route::current()?
        .perform(
            "terminal.write",
            json!({"terminal_id":terminal,"data":text}),
        )
        .await?;
    Ok(())
}

pub(super) async fn write_attached(
    directory: &Path,
    lease: &str,
    backend: u64,
    terminal: &str,
    data: &str,
) -> crate::Result<()> {
    let route = Route::current()?;
    let Route::Attached(a) = &route else {
        return Err("Session frontend detached".into());
    };
    if a.directory != directory || a.lease != lease {
        return Err("Session frontend was replaced".into());
    }
    let (observed, generation) = a
        .input_generation
        .get_or_try_init(|| async {
            let generation = route
                .request(Operation::Capabilities {})
                .await?
                .generation
                .ok_or("Session is connecting")?;
            let snapshot = route
                .request(Operation::State {
                    generation: generation.clone(),
                    resource: Id::new("runtime")?,
                    query: json!({}),
                })
                .await?;
            let ResponseBody::State(state) = snapshot.body else {
                return Err("Missing session state".into());
            };
            let backend = state.value["backend_generation"]
                .as_str()
                .ok_or("Missing backend generation")?
                .parse::<u64>()?;
            Ok::<_, crate::Error>((backend, generation))
        })
        .await?;
    if *observed != backend {
        return Err("The backend changed; attach again".into());
    }
    route
        .request(Operation::Command {
            generation: generation.clone(),
            command: Id::new("terminal.write")?,
            arguments: json!({"terminal_id":terminal,"data":data}),
        })
        .await?;
    Ok(())
}

static NEW_TARGETS: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());

pub(crate) fn submit_session_command(
    command: &'static str,
    arguments: Value,
) -> std::result::Result<(), String> {
    let route = Route::current().map_err(|e| e.to_string())?;
    let handle = match &route {
        Route::Embedded(_) => crate::client::interface::harness::local_runtime()?.1,
        Route::Attached(a) => a.handle.clone(),
    };
    handle.spawn(async move {
        let result: crate::Result<String> = async {
            let value = route.perform(command, arguments).await?;
            if command == "session.cd" {
                return Ok(format!(
                    "Current directory · {}",
                    value["cwd"].as_str().ok_or("Missing current directory")?
                ));
            }
            let target = value["session"]
                .as_str()
                .ok_or("Missing new session target")?
                .to_owned();
            drop(route);
            wait_until_ready(&target).await?;
            NEW_TARGETS
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_back(target);
            Ok("Opening session…".into())
        }
        .await;
        crate::client::render::message(&result.unwrap_or_else(|e| e.to_string()));
    });
    Ok(())
}

async fn wait_until_ready(target: &str) -> crate::Result<()> {
    let observer = RemoteFrontend::connect_local(target, true).await?;
    let mut interval = tokio::time::interval(Duration::from_millis(150));
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            interval.tick().await;
            let response = observer
                .request(Request {
                    version: zbridge::VERSION,
                    id: Id::new("tui-ready")?,
                    operation: Operation::Capabilities {},
                })
                .await?;
            match response.body {
                ResponseBody::Capabilities(c)
                    if c.resources.iter().any(|s| s.id.as_str() == "terminals") =>
                {
                    return Ok::<_, crate::Error>(());
                }
                ResponseBody::Error(error) => return Err(error.into()),
                _ => {}
            }
        }
    })
    .await
    .map_err(|_| format!("Session is still connecting. Its attach target is {target}"))??;
    Ok(())
}

// The keyboard pump awaits the foreground attach. Its child is the only input
// consumer; Ctrl-] detaches and resumes the parent at the same draft and panel.
pub(crate) async fn open_pending_session() {
    let Some(target) = NEW_TARGETS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .pop_front()
    else {
        return;
    };
    let result: crate::Result<()> = async {
        let _screen = crate::client::terminal::lend_screen().await?;
        let mut child = tokio::process::Command::new(std::env::current_exe()?).args(["attach",&target])
            .kill_on_drop(true).spawn()?;
        let attached = ATTACHED.lock().unwrap_or_else(|e|e.into_inner()).clone();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
        loop {
            tokio::select! {
                result = child.wait() => {
                    let status = result?;
                    if !status.success(){return Err(format!("Session frontend exited with {status}").into());}
                    return Ok(())
                }
                _ = heartbeat.tick(), if attached.is_some() => {
                    if let Some(a)=&attached { Route::Attached(a.clone()).request(Operation::Capabilities{}).await?; }
                }
            }
        }
    }.await;
    if let Err(error) = result {
        crate::client::render::status("session", "Error", &error.to_string());
    }
}
