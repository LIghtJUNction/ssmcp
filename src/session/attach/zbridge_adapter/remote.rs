//! An explicit connection to an existing private attach endpoint. The endpoint
//! descriptor and its token are read by the established IPC implementation.
use serde_json::json;
use std::{path::PathBuf, time::Duration};
use zbridge::{BridgeError, ErrorCode, Request, Response};

/// A typed client for an existing ssmcp attach service. Connecting neither starts
/// a service nor launches a model. The lease is private and cannot be imported
/// from request JSON. Call `close` to observe its release acknowledgement.
pub struct RemoteFrontend {
    directory: PathBuf,
    lease: Option<String>,
    read_only: bool,
}

impl RemoteFrontend {
    /// Resolve exactly the local target selected by a frontend. Foreground
    /// endpoints are checked against the PID from the current inventory.
    pub async fn connect_local(target: &str, read_only: bool) -> crate::Result<Self> {
        if target.starts_with("live:") {
            let (directory, _, pid) = crate::session::overview::foreground_target(target)?;
            Self::connect_for_pid(directory, read_only, Some(pid)).await
        } else {
            Self::connect_named(target.strip_prefix("saved:").unwrap_or(target), read_only).await
        }
    }

    /// Resolve an existing saved session through the core's validated manifest.
    /// This does not start a session, disclose its descriptor, or import a lease.
    pub async fn connect_named(name: &str, read_only: bool) -> crate::Result<Self> {
        let root = crate::session::session_root(false)?;
        let (directory, _) = crate::session::load(&root, name)?;
        Self::connect(directory, read_only).await
    }

    pub async fn connect(directory: PathBuf, read_only: bool) -> crate::Result<Self> {
        Self::connect_for_pid(directory, read_only, None).await
    }

    pub(in crate::session) async fn connect_for_pid(
        directory: PathBuf,
        read_only: bool,
        expected_pid: Option<u32>,
    ) -> crate::Result<Self> {
        let open = crate::session::ipc::request_for_pid(
            &directory,
            super::super::ENDPOINT,
            json!({"op":"open","read_only":read_only}),
            crate::session::ipc::REQUEST_TIMEOUT,
            expected_pid,
        )
        .await?;
        let lease = open["lease"]
            .as_str()
            .filter(|value| {
                !value.is_empty()
                    && value.len() <= 128
                    && value.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
            .ok_or("invalid attach frontend lease response")?
            .to_owned();
        let frontend = Self {
            directory,
            lease: Some(lease),
            read_only,
        };
        if open["read_only"] != read_only {
            return Err("attach frontend mode did not match its explicit request".into());
        }
        Ok(frontend)
    }

    pub async fn request(&self, request: Request) -> Result<Response, BridgeError> {
        request.validate()?;
        zbridge::encoded_len(&request, zbridge::MAX_FRAME_BYTES - 1024)?;
        let lease = self
            .lease
            .as_ref()
            .ok_or_else(|| BridgeError::new(ErrorCode::Closed, "attach frontend is detached"))?;
        let command = request.operation.is_command();
        if command && self.read_only {
            return Err(BridgeError::new(
                ErrorCode::Denied,
                "attach frontend is read-only",
            ));
        }
        let result = crate::session::ipc::request(
            &self.directory,
            super::super::ENDPOINT,
            json!({"op":"bridge","lease":lease,"request":&request}),
        )
        .await
        .map_err(|error| BridgeError::new(ErrorCode::Unavailable, error.to_string()));
        let result = result.and_then(|value| {
            zbridge::encoded_len(&value, zbridge::MAX_FRAME_BYTES - 1024)?;
            let response: Response = serde_json::from_value(value)
                .map_err(|_| BridgeError::invalid("invalid attach bridge response"))?;
            response.validate_for(&request)?;
            Ok(response)
        });
        // An outer transport failure cannot establish whether the server already
        // acted. Never replace it with an effect-free error or retry it here.
        result.map_err(|error| if command { error.uncertain() } else { error })
    }

    pub async fn close(&mut self) -> crate::Result<()> {
        let Some(lease) = self.lease.as_ref() else {
            return Ok(());
        };
        let reply = crate::session::ipc::request(
            &self.directory,
            super::super::ENDPOINT,
            json!({"op":"detach","lease":lease}),
        )
        .await?;
        if !reply
            .get("detached")
            .is_some_and(serde_json::Value::is_boolean)
            || reply["session_stopped"] != false
        {
            return Err("invalid attach detach acknowledgement".into());
        }
        self.lease = None;
        Ok(())
    }
}

impl Drop for RemoteFrontend {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let directory = self.directory.clone();
        // Best-effort release only. If no runtime remains or transport failed,
        // the existing attach lease expires after its normal bounded lifetime.
        runtime.spawn(async move {
            let _ = crate::session::ipc::request_with_timeout(
                &directory,
                super::super::ENDPOINT,
                json!({"op":"detach","lease":lease}),
                Duration::from_secs(2),
            )
            .await;
        });
    }
}
