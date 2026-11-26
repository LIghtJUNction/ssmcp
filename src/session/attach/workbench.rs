//! Dispatch TUI session commands through the same zbridge host as GUI clients.
use super::*;
use zbridge::{Id, Operation, Request, Response, ResponseBody};

#[derive(Clone)]
struct Attached {
    directory: PathBuf,
    lease: String,
    read_only: bool,
    handle: tokio::runtime::Handle,
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

    async fn command(&self, command: &'static str, arguments: Value) -> crate::Result<String> {
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
            return Err("Missing session command receipt".into());
        };
        if command == "session.new" {
            let target = result.value["session"]
                .as_str()
                .ok_or("Missing new session target")?;
            Ok(format!(
                "Opened a new session. Attach with ssmcp attach {target}"
            ))
        } else {
            let cwd = result.value["cwd"]
                .as_str()
                .ok_or("Missing current directory")?;
            Ok(format!("Current directory · {cwd}"))
        }
    }
}

pub(crate) fn submit_session_command(
    command: &'static str,
    arguments: Value,
) -> std::result::Result<(), String> {
    let attached = ATTACHED.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let (route, handle) = if let Some(attached) = attached {
        if attached.read_only {
            return Err("This session is read-only".into());
        }
        let handle = attached.handle.clone();
        (Route::Attached(attached), handle)
    } else {
        let (_, handle) = crate::client::interface::harness::local_runtime()?;
        let capture = capture().ok_or("This session has no local frontend")?;
        (
            Route::Embedded(Frontend::open(&capture, false).map_err(|e| e.to_string())?),
            handle,
        )
    };
    handle.spawn(async move {
        let text = route
            .command(command, arguments)
            .await
            .unwrap_or_else(|error| error.to_string());
        crate::client::render::message(&text);
    });
    Ok(())
}
