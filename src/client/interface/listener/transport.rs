//! Linux same-UID adapter over the exact Hub used by authenticated MCP calls.
//! One length-prefixed JSON request per connection; no independent queue.
use super::{Caller, Hub, Owner, random};
use crate::local_socket;
use serde_json::{Value, json};
use std::{
    fs, io,
    os::unix::fs::{DirBuilderExt, MetadataExt},
    path::PathBuf,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    runtime::Handle,
    task::{JoinHandle, JoinSet},
};

const MAX_REQUEST: usize = 4096;
const MAX_RESPONSE: usize = 256 * 1024;
const MAX_CONNECTIONS: usize = 16;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);

fn task() -> &'static Mutex<Option<JoinHandle<()>>> {
    static TASK: OnceLock<Mutex<Option<JoinHandle<()>>>> = OnceLock::new();
    TASK.get_or_init(|| Mutex::new(None))
}

pub(super) fn stop() {
    if let Some(task) = task()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
    {
        task.abort();
    }
}

pub(super) fn replace(hub: Arc<Hub>, id: &str, generation: u64, handle: Option<Handle>) {
    stop();
    let Some(handle) = handle else { return };
    let id = id.to_owned();
    *task().lock().unwrap_or_else(|error| error.into_inner()) = Some(handle.spawn(async move {
        // A failed local endpoint leaves the authenticated network listener usable.
        // Startup errors do not expose paths or turn into an anonymous fallback.
        if let Err(error) = run(hub, id, generation).await {
            crate::client::terminal::notice(&format!("Local listener unavailable: {error}"));
        }
    }));
}

struct Directory {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl Directory {
    fn create() -> io::Result<Self> {
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        let root = PathBuf::from(format!("/tmp/ssmcp-{uid}"));
        match fs::DirBuilder::new().mode(0o700).create(&root) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        // Check every ancestor with O_NOFOLLOW before creating our owned child.
        local_socket::SocketPath::open(&root.join("check.sock"))?;
        let suffix = random(8).map_err(io::Error::other)?;
        let path = root.join(format!("listener-{}-{suffix}", std::process::id()));
        fs::DirBuilder::new().mode(0o700).create(&path)?;
        let metadata = fs::symlink_metadata(&path)?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn socket(&self) -> PathBuf {
        self.path.join("receiver.sock")
    }
}

impl Drop for Directory {
    fn drop(&mut self) {
        // Bound drops first and removes only its socket inode. Remove only this
        // private directory's regular lock; never recurse through arbitrary data.
        if fs::symlink_metadata(&self.path).is_ok_and(|meta| {
            meta.is_dir() && meta.dev() == self.device && meta.ino() == self.inode
        }) {
            let lock = self.path.join("receiver.sock.lock");
            if fs::symlink_metadata(&lock).is_ok_and(|meta| {
                meta.is_file() && meta.nlink() == 1 && meta.mode() & 0o777 == 0o600
            }) {
                let _ = fs::remove_file(lock);
            }
            let _ = fs::remove_dir(&self.path);
        }
    }
}

fn peer(stream: &UnixStream, generation: u64) -> io::Result<Caller> {
    local_socket::peer(stream)?;
    let credentials = stream.peer_cred()?;
    let pid = credentials
        .pid()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0)
        .ok_or_else(|| io::Error::other("missing Unix peer PID"))?;
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let started = stat
        .rsplit_once(") ")
        .and_then(|(_, fields)| fields.split_whitespace().nth(19))
        .and_then(|value| value.parse::<u64>().ok())
        .ok_or_else(|| io::Error::other("missing Unix peer start time"))?;
    Ok(Caller {
        generation,
        session: None,
        owner: Owner::Local {
            uid: credentials.uid(),
            pid,
            started,
        },
    })
}

async fn request(hub: Arc<Hub>, mut stream: UnixStream, generation: u64) -> io::Result<()> {
    let caller = peer(&stream, generation)?;
    let length = stream.read_u32().await? as usize;
    if length == 0 || length > MAX_REQUEST {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "listener request exceeds 4096 bytes",
        ));
    }
    let mut bytes = vec![0; length];
    stream.read_exact(&mut bytes).await?;
    let arguments: Value = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
    let (mut reader, mut writer) = stream.into_split();
    let mut trailing = [0u8; 1];
    let result = tokio::select! {
        result = hub.call_for(caller, arguments) => result,
        _ = reader.read(&mut trailing) => {
            // EOF or extra input cancels this request and drops its Reading guard.
            return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "listener peer ended its request"));
        },
    };
    let response = match result {
        Ok(result) => json!({"ok":true,"result":result}),
        Err(error) => json!({"ok":false,"error":error}),
    };
    let bytes = serde_json::to_vec(&response).map_err(io::Error::other)?;
    if bytes.len() > MAX_RESPONSE {
        return Err(io::Error::other("listener response exceeds bounded frame"));
    }
    writer.write_u32(bytes.len() as u32).await?;
    writer.write_all(&bytes).await?;
    writer.shutdown().await
}

async fn serve(hub: Arc<Hub>, bound: &local_socket::Bound, generation: u64) -> io::Result<()> {
    let mut requests = JoinSet::new();
    loop {
        tokio::select! {
            result = bound.listener.accept() => {
                let (stream, _) = result?;
                if requests.len() >= MAX_CONNECTIONS { continue; }
                let hub = hub.clone();
                requests.spawn(async move {
                    let _ = tokio::time::timeout(REQUEST_TIMEOUT, request(hub, stream, generation)).await;
                });
            },
            _ = requests.join_next(), if !requests.is_empty() => {},
        }
    }
}

struct PublishedEndpoint {
    hub: Arc<Hub>,
    path: String,
}
impl Drop for PublishedEndpoint {
    fn drop(&mut self) {
        let mut state = self
            .hub
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if state.local_endpoint.as_deref() == Some(&self.path) {
            state.local_endpoint = None;
        }
        drop(state);
        self.hub.changed();
    }
}

async fn run(hub: Arc<Hub>, id: String, generation: u64) -> io::Result<()> {
    let directory = Directory::create()?;
    let path = directory.socket();
    let bound = local_socket::Bound::bind(&path).await?;
    {
        let mut state = hub.state.lock().unwrap_or_else(|error| error.into_inner());
        if !state
            .connection
            .as_ref()
            .is_some_and(|connection| connection.id == id && connection.generation == generation)
        {
            return Ok(());
        }
        state.local_endpoint = Some(path.to_string_lossy().into_owned());
    }
    let _published = PublishedEndpoint {
        hub: hub.clone(),
        path: path.to_string_lossy().into_owned(),
    };
    hub.changed();
    serve(hub, &bound, generation).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    async fn exchange(path: &Path, request: Value) -> Value {
        let mut stream = UnixStream::connect(path).await.unwrap();
        let bytes = serde_json::to_vec(&request).unwrap();
        stream.write_u32(bytes.len() as u32).await.unwrap();
        stream.write_all(&bytes).await.unwrap();
        let length = stream.read_u32().await.unwrap() as usize;
        assert!(length <= MAX_RESPONSE);
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn unix_listener_uses_the_shared_lease_and_releases_cancelled_reads() {
        let hub = Arc::new(Hub::default());
        hub.reset_for(Some("unix_fixture"), 7);
        let directory = Directory::create().unwrap();
        let path = directory.socket();
        let bound = local_socket::Bound::bind(&path).await.unwrap();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let server = tokio::spawn({
            let hub = hub.clone();
            async move { serve(hub, &bound, 7).await }
        });
        let opened = exchange(
            &path,
            json!({"op":"open","label":"local fixture","idempotency_key":"local_1"}),
        )
        .await;
        assert_eq!(opened["ok"], true);
        let id = opened["result"]["listener_id"].as_str().unwrap();
        let token = opened["result"]["token"].as_str().unwrap();
        let replay = exchange(
            &path,
            json!({"op":"open","label":"local fixture","idempotency_key":"local_1"}),
        )
        .await;
        assert_eq!(replay["result"]["token"], token);
        let mut stream = UnixStream::connect(&path).await.unwrap();
        let bytes =
            serde_json::to_vec(&json!({"op":"read","listener_id":id,"token":token,"wait_ms":5000}))
                .unwrap();
        stream.write_u32(bytes.len() as u32).await.unwrap();
        stream.write_all(&bytes).await.unwrap();
        tokio::time::timeout(Duration::from_secs(1), async {
            while hub.snapshot()["listener"]["reading"] != true {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        drop(stream);
        tokio::time::timeout(Duration::from_secs(1), async {
            while hub.snapshot()["listener"]["reading"] != false {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let ticket = hub
            .submit(
                hub.target_for(None).unwrap(),
                "source".into(),
                "body".into(),
            )
            .unwrap();
        let read = exchange(
            &path,
            json!({"op":"read","listener_id":id,"token":token,"wait_ms":0}),
        )
        .await;
        assert_eq!(read["result"]["delivery"]["cursor"], 1);
        assert_eq!(read["result"]["delivery"]["text"], "body");
        let receipt = exchange(&path, json!({"op":"ack","listener_id":id,"token":token,"delivery_id":read["result"]["delivery"]["delivery_id"],"status":"accepted"})).await;
        assert_eq!(receipt["result"]["receipt"]["cursor"], 1);
        assert_eq!(
            ticket.wait().await.unwrap().status,
            super::super::Status::Accepted
        );
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
        assert!(!path.exists());
        drop(directory);
        assert!(!path.parent().unwrap().exists());
    }
}
