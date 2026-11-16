//! Root-scoped files and bounded loopback TCP handles shared by every backend.
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    ffi::CString,
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::{Component, Path, PathBuf},
    sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{
        TcpListener, TcpStream,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, watch},
    time::timeout,
};

pub const MAX_FILE_BYTES: usize = 256 * 1024;
pub const MAX_NET_BYTES: usize = 64 * 1024;
pub const MAX_HANDLES: usize = 64;
pub const MAX_WAIT_MS: u64 = 5_000;
const MAX_ENTRIES: usize = 128;

struct Root {
    path: PathBuf,
    directory: File,
}
struct Listener {
    socket: TcpListener,
    closed: watch::Sender<bool>,
    _permit: OwnedSemaphorePermit,
}
struct Stream {
    reader: Mutex<OwnedReadHalf>,
    writer: Mutex<OwnedWriteHalf>,
    address: SocketAddr,
    peer: SocketAddr,
    closed: watch::Sender<bool>,
    write_closed: AtomicBool,
    _permit: OwnedSemaphorePermit,
}
#[derive(Clone)]
enum Handle {
    Listener(Arc<Listener>),
    Stream(Arc<Stream>),
}
impl Handle {
    fn close(&self) {
        match self {
            Self::Listener(value) => {
                value.closed.send_replace(true);
            }
            Self::Stream(value) => {
                value.closed.send_replace(true);
            }
        }
    }
    fn description(&self, id: &str) -> Value {
        match self {
            Self::Listener(value) => {
                json!({"id":id,"kind":"listener","address":value.socket.local_addr().ok().map(|a|a.to_string())})
            }
            Self::Stream(value) => {
                json!({"id":id,"kind":"stream","address":value.address.to_string(),"peer":value.peer.to_string(),"write_closed":value.write_closed.load(Ordering::Acquire)})
            }
        }
    }
}

/// Handles belong to one live SSH client and never grant access to another workspace.
pub struct Workspace {
    root: Arc<Root>,
    handles: StdMutex<HashMap<String, Handle>>,
    slots: Arc<Semaphore>,
    nonce: String,
    next: AtomicU64,
    closed: AtomicBool,
}

impl Workspace {
    pub fn new(root: PathBuf) -> Result<Self, String> {
        let path = root.canonicalize().map_err(error)?;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .map_err(error)?;
        Ok(Self {
            root: Arc::new(Root { path, directory }),
            handles: StdMutex::new(HashMap::new()),
            slots: Arc::new(Semaphore::new(MAX_HANDLES)),
            nonce: random_id().map_err(error)?,
            next: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root.path
    }

    pub fn tools() -> Vec<Value> {
        let path = json!({"type":"string","maxLength":4096,"description":"Path relative to this client's workspace root; symlinks and parent traversal are rejected."});
        let id = json!({"type":"string","maxLength":128});
        let wait = json!({"type":"integer","minimum":0,"maximum":MAX_WAIT_MS,"default":0});
        let host =
            json!({"type":"string","enum":["127.0.0.1","::1","localhost"],"default":"127.0.0.1"});
        let mut tools = vec![
            tool(
                "ssmcp.fs.list",
                "List a bounded page of directory entries inside the workspace root.",
                json!({"path":path,"cursor":{"type":"string","maxLength":128},"max_entries":{"type":"integer","minimum":1,"maximum":MAX_ENTRIES,"default":64}}),
                &[],
                true,
            ),
            tool(
                "ssmcp.fs.stat",
                "Read workspace root and metadata for one relative path.",
                json!({"path":path}),
                &[],
                true,
            ),
            tool(
                "ssmcp.fs.read",
                "Read a bounded binary file chunk as base64; reuse next_offset until eof.",
                json!({"path":path,"offset":{"type":"integer","minimum":0,"default":0},"max_bytes":{"type":"integer","minimum":1,"maximum":MAX_FILE_BYTES,"default":65536}}),
                &["path"],
                true,
            ),
            tool(
                "ssmcp.fs.write",
                "Atomically write one bounded file. Existing files require overwrite=true. Use rsync for larger files.",
                json!({"path":path,"data_base64":{"type":"string","maxLength":MAX_FILE_BYTES.div_ceil(3)*4},"overwrite":{"type":"boolean","default":false},"create_parents":{"type":"boolean","default":false}}),
                &["path", "data_base64"],
                false,
            ),
            tool(
                "ssmcp.fs.mkdir",
                "Create a workspace-relative directory.",
                json!({"path":path,"parents":{"type":"boolean","default":false}}),
                &["path"],
                false,
            ),
            tool(
                "ssmcp.net.listen",
                "Bind a loopback TCP listener. Port 0 selects an available port.",
                json!({"host":host,"port":{"type":"integer","minimum":0,"maximum":65535,"default":0}}),
                &[],
                false,
            ),
            tool(
                "ssmcp.net.accept",
                "Accept one connection; stream=null means the wait expired.",
                json!({"id":id,"wait_ms":wait}),
                &["id"],
                false,
            ),
            tool(
                "ssmcp.net.connect",
                "Connect to an explicit loopback TCP endpoint.",
                json!({"host":host,"port":{"type":"integer","minimum":1,"maximum":65535},"wait_ms":{"type":"integer","minimum":1,"maximum":MAX_WAIT_MS,"default":5000}}),
                &["port"],
                false,
            ),
            tool(
                "ssmcp.net.read",
                "Read a binary chunk. Empty data with timed_out=true is not EOF.",
                json!({"id":id,"max_bytes":{"type":"integer","minimum":1,"maximum":MAX_NET_BYTES,"default":32768},"wait_ms":wait}),
                &["id"],
                false,
            ),
            tool(
                "ssmcp.net.write",
                "Write bounded base64 bytes; written records progress even if the socket stalls.",
                json!({"id":id,"data_base64":{"type":"string","maxLength":MAX_NET_BYTES.div_ceil(3)*4}}),
                &["id", "data_base64"],
                false,
            ),
            tool(
                "ssmcp.net.shutdown",
                "Half-close a TCP stream's write side while keeping its read side open.",
                json!({"id":id}),
                &["id"],
                false,
            ),
            tool(
                "ssmcp.net.close",
                "Close a listener or stream and cancel its pending operations. Repeated closes are harmless.",
                json!({"id":id}),
                &["id"],
                false,
            ),
            tool(
                "ssmcp.net.list",
                "List this client's active loopback listeners and streams.",
                json!({}),
                &[],
                true,
            ),
        ];
        for item in &mut tools {
            item["annotations"]["openWorldHint"] = json!(false);
        }
        tools
    }

    pub async fn perform(&self, name: &str, arguments: Value) -> Result<Value, String> {
        if self.closed.load(Ordering::Acquire) {
            return Err("workspace is closed".into());
        }
        if name.starts_with("ssmcp.fs.") {
            let root = self.root.clone();
            let name = name.to_owned();
            return tokio::task::spawn_blocking(move || root.perform(&name, arguments))
                .await
                .map_err(|e| e.to_string())?;
        }
        match name {
            "ssmcp.net.listen" => self.listen(parse(arguments)?).await,
            "ssmcp.net.accept" => self.accept(parse(arguments)?).await,
            "ssmcp.net.connect" => self.connect(parse(arguments)?).await,
            "ssmcp.net.read" => self.read(parse(arguments)?).await,
            "ssmcp.net.write" => self.write(parse(arguments)?).await,
            "ssmcp.net.shutdown" => self.half_close(parse(arguments)?).await,
            "ssmcp.net.close" => {
                let args: HandleArgs = parse(arguments)?;
                let handle = self.handles.lock().unwrap().remove(&args.id);
                if let Some(handle) = &handle {
                    handle.close();
                }
                Ok(json!({"closed":handle.is_some()}))
            }
            "ssmcp.net.list" => {
                let _: Empty = parse(arguments)?;
                let map = self.handles.lock().unwrap();
                let mut handles: Vec<_> = map.iter().map(|(id, h)| h.description(id)).collect();
                handles.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
                Ok(json!({"handles":handles,"max_handles":MAX_HANDLES}))
            }
            _ => Err(format!("unknown workspace capability: {name}")),
        }
    }

    fn permit(&self) -> Result<OwnedSemaphorePermit, String> {
        self.slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| format!("workspace TCP handle limit is {MAX_HANDLES}"))
    }
    fn insert(&self, handle: Handle) -> Result<String, String> {
        let mut handles = self.handles.lock().unwrap();
        if self.closed.load(Ordering::Acquire) {
            handle.close();
            return Err("workspace is closed".into());
        }
        let id = format!(
            "{}-{:x}",
            self.nonce,
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        handles.insert(id.clone(), handle);
        Ok(id)
    }
    fn stream(&self, id: &str) -> Result<Arc<Stream>, String> {
        if id.len() > 128 {
            return Err("invalid stream id".into());
        }
        match self.handles.lock().unwrap().get(id) {
            Some(Handle::Stream(value)) => Ok(value.clone()),
            _ => Err("unknown TCP stream".into()),
        }
    }
    fn register_stream(
        &self,
        socket: TcpStream,
        permit: OwnedSemaphorePermit,
    ) -> Result<Value, String> {
        socket.set_nodelay(true).map_err(error)?;
        let address = socket.local_addr().map_err(error)?;
        let peer = socket.peer_addr().map_err(error)?;
        let (reader, writer) = socket.into_split();
        let (closed, _) = watch::channel(false);
        let id = self.insert(Handle::Stream(Arc::new(Stream {
            reader: Mutex::new(reader),
            writer: Mutex::new(writer),
            address,
            peer,
            closed,
            write_closed: AtomicBool::new(false),
            _permit: permit,
        })))?;
        Ok(json!({"id":id,"address":address.to_string(),"peer":peer.to_string()}))
    }
    async fn listen(&self, args: ListenArgs) -> Result<Value, String> {
        let address = SocketAddr::new(loopback(&args.host)?, args.port);
        let permit = self.permit()?;
        let socket = TcpListener::bind(address).await.map_err(error)?;
        let address = socket.local_addr().map_err(error)?;
        let (closed, _) = watch::channel(false);
        let id = self.insert(Handle::Listener(Arc::new(Listener {
            socket,
            closed,
            _permit: permit,
        })))?;
        Ok(json!({"id":id,"address":address.to_string(),"port":address.port()}))
    }
    async fn accept(&self, args: AcceptArgs) -> Result<Value, String> {
        check_wait(args.wait_ms)?;
        let listener = match self.handles.lock().unwrap().get(&args.id) {
            Some(Handle::Listener(value)) => value.clone(),
            _ => return Err("unknown TCP listener".into()),
        };
        let permit = self.permit()?;
        let mut closed = listener.closed.subscribe();
        if *closed.borrow() {
            return Err("TCP listener is closed".into());
        }
        let accepted = tokio::select! {
            biased;
            _=closed.changed()=>return Err("TCP listener is closed".into()),
            result=timeout(Duration::from_millis(args.wait_ms),listener.socket.accept())=>result,
        };
        match accepted {
            Ok(Ok((socket, _))) => Ok(json!({"stream":self.register_stream(socket,permit)?})),
            Ok(Err(e)) => Err(error(e)),
            Err(_) => Ok(json!({"stream":null,"timed_out":true})),
        }
    }
    async fn connect(&self, args: ConnectArgs) -> Result<Value, String> {
        if args.port == 0 || args.wait_ms == 0 {
            return Err("port and wait_ms must be positive".into());
        }
        check_wait(args.wait_ms)?;
        let address = SocketAddr::new(loopback(&args.host)?, args.port);
        let permit = self.permit()?;
        let socket = timeout(
            Duration::from_millis(args.wait_ms),
            TcpStream::connect(address),
        )
        .await
        .map_err(|_| "TCP connection timed out")?
        .map_err(error)?;
        self.register_stream(socket, permit)
    }
    async fn read(&self, args: NetReadArgs) -> Result<Value, String> {
        check_wait(args.wait_ms)?;
        if args.max_bytes == 0 || args.max_bytes > MAX_NET_BYTES {
            return Err(format!("max_bytes must be 1..={MAX_NET_BYTES}"));
        }
        let stream = self.stream(&args.id)?;
        let mut closed = stream.closed.subscribe();
        if *closed.borrow() {
            return Err("TCP stream is closed".into());
        }
        let mut bytes = vec![0; args.max_bytes];
        let result = tokio::select! {
            biased;
            _=closed.changed()=>return Err("TCP stream is closed".into()),
            result=timeout(Duration::from_millis(args.wait_ms),async {
                stream.reader.lock().await.read(&mut bytes).await
            })=>result,
        };
        match result {
            Ok(Ok(count)) => Ok(
                json!({"data_base64":STANDARD.encode(&bytes[..count]),"eof":count==0,"timed_out":false}),
            ),
            Ok(Err(e)) => Err(error(e)),
            Err(_) => Ok(json!({"data_base64":"","eof":false,"timed_out":true})),
        }
    }
    async fn write(&self, args: NetWriteArgs) -> Result<Value, String> {
        let data = decode(&args.data_base64, MAX_NET_BYTES)?;
        let stream = self.stream(&args.id)?;
        let mut closed = stream.closed.subscribe();
        if *closed.borrow() {
            return Err("TCP stream is closed".into());
        }
        if stream.write_closed.load(Ordering::Acquire) {
            return Err("TCP write side is shut down".into());
        }
        let mut written = 0;
        let result = tokio::select! {
            biased;
            _=closed.changed()=>return Err("TCP stream is closed".into()),
            result=timeout(Duration::from_millis(MAX_WAIT_MS),async {
                let mut writer=stream.writer.lock().await;
                while written<data.len() {
                    let count=writer.write(&data[written..]).await?;
                    if count==0 { return Err(io::Error::new(io::ErrorKind::WriteZero,"TCP write returned zero")); }
                    written+=count;
                }
                Ok::<_,io::Error>(())
            })=>result,
        };
        match result {
            Ok(Ok(())) => Ok(json!({"written":written,"timed_out":false})),
            Ok(Err(e)) => Ok(json!({"written":written,"timed_out":false,"error":error(e)})),
            Err(_) => Ok(json!({"written":written,"timed_out":true})),
        }
    }
    async fn half_close(&self, args: HandleArgs) -> Result<Value, String> {
        let stream = self.stream(&args.id)?;
        let mut closed = stream.closed.subscribe();
        if *closed.borrow() {
            return Err("TCP stream is closed".into());
        }
        tokio::select! {
            biased;
            _=closed.changed()=>return Err("TCP stream is closed".into()),
            result=timeout(Duration::from_millis(MAX_WAIT_MS),async {
                let mut writer=stream.writer.lock().await;
                writer.shutdown().await?;
                stream.write_closed.store(true,Ordering::Release);
                Ok::<_,io::Error>(())
            })=>result.map_err(|_|"TCP shutdown timed out")?.map_err(error)?,
        }
        Ok(json!({"write_closed":true}))
    }
    pub async fn shutdown(&self) {
        self.close_all();
    }
    fn close_all(&self) {
        let mut handles = self.handles.lock().unwrap();
        self.closed.store(true, Ordering::Release);
        for (_, handle) in handles.drain() {
            handle.close();
        }
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        self.close_all();
    }
}

impl Root {
    fn perform(&self, name: &str, args: Value) -> Result<Value, String> {
        match name {
            "ssmcp.fs.list" => self.list(parse(args)?),
            "ssmcp.fs.stat" => self.stat(parse(args)?),
            "ssmcp.fs.read" => self.read(parse(args)?),
            "ssmcp.fs.write" => self.write(parse(args)?),
            "ssmcp.fs.mkdir" => self.mkdir(parse(args)?),
            _ => Err(format!("unknown workspace capability: {name}")),
        }
    }
    fn components(path: &str) -> Result<Vec<CString>, String> {
        if path.len() > 4096 || path.chars().any(char::is_control) {
            return Err("invalid workspace path".into());
        }
        Path::new(path)
            .components()
            .filter_map(|component| match component {
                Component::CurDir => None,
                Component::Normal(name) => Some(
                    CString::new(name.as_encoded_bytes())
                        .map_err(|_| "invalid workspace path".into()),
                ),
                _ => Some(Err("path must be relative without parent traversal".into())),
            })
            .collect()
    }
    fn directory(&self, parts: &[CString], create: bool) -> Result<File, String> {
        let mut directory = self.directory.try_clone().map_err(error)?;
        for part in parts {
            let mut next = open_at(&directory, part, libc::O_RDONLY | libc::O_DIRECTORY, 0);
            if create
                && next
                    .as_ref()
                    .is_err_and(|e| e.kind() == io::ErrorKind::NotFound)
            {
                let result = unsafe { libc::mkdirat(directory.as_raw_fd(), part.as_ptr(), 0o700) };
                if result != 0 && io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists
                {
                    return Err(error(io::Error::last_os_error()));
                }
                next = open_at(&directory, part, libc::O_RDONLY | libc::O_DIRECTORY, 0);
            }
            directory = next.map_err(error)?;
        }
        Ok(directory)
    }
    fn file(&self, path: &str) -> Result<File, String> {
        let parts = Self::components(path)?;
        let Some((name, parents)) = parts.split_last() else {
            return self.directory.try_clone().map_err(error);
        };
        open_at(
            &self.directory(parents, false)?,
            name,
            libc::O_RDONLY | libc::O_NONBLOCK,
            0,
        )
        .map_err(error)
    }
    fn stat(&self, args: PathArgs) -> Result<Value, String> {
        let metadata = self.file(&args.path)?.metadata().map_err(error)?;
        Ok(
            json!({"root":self.path.to_string_lossy(),"path":args.path,"kind":kind(&metadata),"size":metadata.len(),"modified_ns":modified(&metadata),"mode":metadata.mode()&0o777}),
        )
    }
    fn list(&self, args: ListArgs) -> Result<Value, String> {
        if args.max_entries == 0 || args.max_entries > MAX_ENTRIES {
            return Err(format!("max_entries must be 1..={MAX_ENTRIES}"));
        }
        let directory = self.directory(&Self::components(&args.path)?, false)?;
        let metadata = directory.metadata().map_err(error)?;
        let stamp = format!(
            "{:x}-{:x}-{:x}",
            metadata.ino(),
            metadata.mtime(),
            metadata.mtime_nsec()
        );
        let offset = if let Some(cursor) = args.cursor {
            if cursor.len() > 128 {
                return Err("invalid directory cursor".into());
            }
            let (version, index) = cursor.split_once(':').ok_or("invalid directory cursor")?;
            if version != stamp {
                return Err("directory changed; restart without cursor".into());
            }
            index
                .parse::<usize>()
                .map_err(|_| "invalid directory cursor")?
        } else {
            0
        };
        // This path references the already-open directory descriptor, not a user-controlled symlink.
        let mut iterator =
            std::fs::read_dir(format!("/proc/self/fd/{}", directory.as_raw_fd())).map_err(error)?;
        for _ in 0..offset {
            if iterator.next().transpose().map_err(error)?.is_none() {
                return Err("directory cursor is beyond the entries".into());
            }
        }
        let mut entries = Vec::new();
        for _ in 0..args.max_entries {
            let Some(entry) = iterator.next().transpose().map_err(error)? else {
                break;
            };
            let metadata = entry.metadata().map_err(error)?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| "directory contains a non-UTF-8 filename")?;
            entries.push(json!({"name":name,"kind":kind(&metadata),"size":metadata.len()}));
        }
        let more = iterator.next().transpose().map_err(error)?.is_some();
        let current = directory.metadata().map_err(error)?;
        if modified(&metadata) != modified(&current) {
            return Err("directory changed; restart without cursor".into());
        }
        let mut result =
            json!({"root":self.path.to_string_lossy(),"path":args.path,"entries":entries});
        if more {
            result["next_cursor"] = json!(format!(
                "{stamp}:{}",
                offset + result["entries"].as_array().unwrap().len()
            ));
        }
        Ok(result)
    }
    fn read(&self, args: FileReadArgs) -> Result<Value, String> {
        if args.max_bytes == 0 || args.max_bytes > MAX_FILE_BYTES {
            return Err(format!("max_bytes must be 1..={MAX_FILE_BYTES}"));
        }
        let mut file = self.file(&args.path)?;
        let metadata = file.metadata().map_err(error)?;
        if !metadata.is_file() {
            return Err("path is not a regular file".into());
        }
        if args.offset > metadata.len() {
            return Err("offset is beyond the file".into());
        }
        file.seek(SeekFrom::Start(args.offset)).map_err(error)?;
        let mut bytes = vec![0; args.max_bytes];
        let mut count = 0;
        while count < bytes.len() {
            let read = file.read(&mut bytes[count..]).map_err(error)?;
            if read == 0 {
                break;
            }
            count += read;
        }
        let next = args.offset + count as u64;
        Ok(
            json!({"data_base64":STANDARD.encode(&bytes[..count]),"offset":args.offset,"next_offset":next,"size":metadata.len(),"eof":next>=metadata.len()}),
        )
    }
    fn write(&self, args: FileWriteArgs) -> Result<Value, String> {
        let data = decode(&args.data_base64, MAX_FILE_BYTES)?;
        let parts = Self::components(&args.path)?;
        let (name, parents) = parts.split_last().ok_or("write requires a file path")?;
        let parent = self.directory(parents, args.create_parents)?;
        // An overwrite may replace only a regular file, never a symlink or device.
        if args.overwrite {
            match open_at(&parent, name, libc::O_RDONLY | libc::O_NONBLOCK, 0) {
                Ok(existing) if !existing.metadata().map_err(error)?.is_file() => {
                    return Err("overwrite target must be a regular file".into());
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(error(e)),
            }
        }
        let temporary =
            CString::new(format!(".ssmcp-{}.tmp", random_id().map_err(error)?)).unwrap();
        let mut file = open_at(
            &parent,
            &temporary,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
        .map_err(error)?;
        struct Temp<'a> {
            directory: &'a File,
            name: &'a CString,
        }
        impl Drop for Temp<'_> {
            fn drop(&mut self) {
                unsafe { libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0) };
            }
        }
        let _temporary = Temp {
            directory: &parent,
            name: &temporary,
        };
        file.write_all(&data).map_err(error)?;
        file.sync_all().map_err(error)?;
        let outcome = if args.overwrite {
            unsafe {
                libc::renameat(
                    parent.as_raw_fd(),
                    temporary.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                )
            }
        } else {
            unsafe {
                libc::linkat(
                    parent.as_raw_fd(),
                    temporary.as_ptr(),
                    parent.as_raw_fd(),
                    name.as_ptr(),
                    0,
                )
            }
        };
        if outcome != 0 {
            return Err(error(io::Error::last_os_error()));
        }
        parent.sync_all().map_err(error)?;
        Ok(json!({"path":args.path,"written":data.len()}))
    }
    fn mkdir(&self, args: MkdirArgs) -> Result<Value, String> {
        let parts = Self::components(&args.path)?;
        if args.parents {
            self.directory(&parts, true)?;
        } else {
            let (name, parents) = parts
                .split_last()
                .ok_or("mkdir requires a directory path")?;
            let parent = self.directory(parents, false)?;
            if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                return Err(error(io::Error::last_os_error()));
            }
        }
        Ok(json!({"path":args.path,"created":true}))
    }
}

fn open_at(directory: &File, name: &CString, flags: i32, mode: u32) -> io::Result<File> {
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            mode,
        )
    };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}
fn random_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}
fn modified(metadata: &std::fs::Metadata) -> String {
    format!("{}:{:09}", metadata.mtime(), metadata.mtime_nsec())
}
fn kind(metadata: &std::fs::Metadata) -> &'static str {
    if metadata.is_file() {
        "file"
    } else if metadata.is_dir() {
        "directory"
    } else if metadata.file_type().is_symlink() {
        "symlink"
    } else {
        "special"
    }
}
fn error(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn parse<T: DeserializeOwned>(args: Value) -> Result<T, String> {
    serde_json::from_value(args).map_err(|e| format!("invalid workspace arguments: {e}"))
}
fn decode(value: &str, limit: usize) -> Result<Vec<u8>, String> {
    if value.len() > limit.div_ceil(3) * 4 {
        return Err(format!("base64 data exceeds {limit} decoded bytes"));
    }
    let data = STANDARD.decode(value).map_err(|_| "invalid base64 data")?;
    if data.len() > limit {
        return Err(format!("data exceeds {limit} bytes"));
    }
    Ok(data)
}
fn loopback(host: &str) -> Result<IpAddr, String> {
    match host {
        "localhost" | "127.0.0.1" => Ok(IpAddr::V4(Ipv4Addr::LOCALHOST)),
        "::1" => Ok(IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)),
        _ => Err("TCP host must be 127.0.0.1, ::1, or localhost".into()),
    }
}
fn check_wait(wait: u64) -> Result<(), String> {
    if wait > MAX_WAIT_MS {
        Err(format!("wait_ms must be 0..={MAX_WAIT_MS}"))
    } else {
        Ok(())
    }
}
fn tool(
    name: &str,
    description: &str,
    properties: Value,
    required: &[&str],
    readonly: bool,
) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":readonly,"destructiveHint":!readonly}})
}
fn default_path() -> String {
    ".".into()
}
fn default_host() -> String {
    "127.0.0.1".into()
}
fn default_entries() -> usize {
    64
}
fn default_file_bytes() -> usize {
    65536
}
fn default_net_bytes() -> usize {
    32768
}
fn default_connect_wait() -> u64 {
    5000
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PathArgs {
    #[serde(default = "default_path")]
    path: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default = "default_path")]
    path: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default = "default_entries")]
    max_entries: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileReadArgs {
    path: String,
    #[serde(default)]
    offset: u64,
    #[serde(default = "default_file_bytes")]
    max_bytes: usize,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileWriteArgs {
    path: String,
    data_base64: String,
    #[serde(default)]
    overwrite: bool,
    #[serde(default)]
    create_parents: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MkdirArgs {
    path: String,
    #[serde(default)]
    parents: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListenArgs {
    #[serde(default = "default_host")]
    host: String,
    #[serde(default)]
    port: u16,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConnectArgs {
    #[serde(default = "default_host")]
    host: String,
    port: u16,
    #[serde(default = "default_connect_wait")]
    wait_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HandleArgs {
    id: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AcceptArgs {
    id: String,
    #[serde(default)]
    wait_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NetReadArgs {
    id: String,
    #[serde(default = "default_net_bytes")]
    max_bytes: usize,
    #[serde(default)]
    wait_ms: u64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NetWriteArgs {
    id: String,
    data_base64: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("ssmcp-workspace-test-{}", random_id().unwrap()));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn workspace(&self) -> Workspace {
            Workspace::new(self.0.clone()).unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    async fn call(workspace: &Workspace, name: &str, args: Value) -> Value {
        workspace.perform(name, args).await.unwrap()
    }
    #[tokio::test]
    async fn files_round_trip_chunks_and_atomic_overwrite() {
        let scratch = Scratch::new();
        let workspace = scratch.workspace();
        let bytes: Vec<u8> = (0..70000).map(|n| (n % 256) as u8).collect();
        call(&workspace,"ssmcp.fs.write",json!({"path":"nested/file","create_parents":true,"data_base64":STANDARD.encode(&bytes)})).await;
        let first = call(
            &workspace,
            "ssmcp.fs.read",
            json!({"path":"nested/file","max_bytes":32768}),
        )
        .await;
        let second = call(
            &workspace,
            "ssmcp.fs.read",
            json!({"path":"nested/file","offset":first["next_offset"],"max_bytes":MAX_FILE_BYTES}),
        )
        .await;
        let mut actual = STANDARD
            .decode(first["data_base64"].as_str().unwrap())
            .unwrap();
        actual.extend(
            STANDARD
                .decode(second["data_base64"].as_str().unwrap())
                .unwrap(),
        );
        assert_eq!(actual, bytes);
        assert_eq!(first["eof"], false);
        assert_eq!(second["eof"], true);
        assert!(
            workspace
                .perform(
                    "ssmcp.fs.write",
                    json!({"path":"nested/file","data_base64":"bmV3"})
                )
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(scratch.0.join("nested/file")).unwrap(), bytes);
        call(
            &workspace,
            "ssmcp.fs.write",
            json!({"path":"nested/file","overwrite":true,"data_base64":"bmV3"}),
        )
        .await;
        assert_eq!(
            std::fs::read(scratch.0.join("nested/file")).unwrap(),
            b"new"
        );
        assert!(
            !std::fs::read_dir(scratch.0.join("nested"))
                .unwrap()
                .any(|e| e
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".ssmcp-"))
        );
    }
    #[tokio::test]
    async fn paths_symlinks_special_files_and_bounds_are_rejected() {
        let scratch = Scratch::new();
        let outside = Scratch::new();
        let workspace = scratch.workspace();
        std::fs::write(outside.0.join("secret"), b"secret").unwrap();
        std::os::unix::fs::symlink(&outside.0, scratch.0.join("escape")).unwrap();
        std::os::unix::fs::symlink(outside.0.join("secret"), scratch.0.join("link")).unwrap();
        for path in ["../secret", "/etc/passwd", "escape/secret", "link"] {
            assert!(
                workspace
                    .perform("ssmcp.fs.read", json!({"path":path}))
                    .await
                    .is_err(),
                "{path}"
            );
        }
        assert!(
            workspace
                .perform(
                    "ssmcp.fs.write",
                    json!({"path":"escape/secret","data_base64":"eA==","overwrite":true})
                )
                .await
                .is_err()
        );
        assert!(
            workspace
                .perform(
                    "ssmcp.fs.write",
                    json!({"path":"link","data_base64":"eA==","overwrite":true})
                )
                .await
                .is_err()
        );
        assert_eq!(std::fs::read(outside.0.join("secret")).unwrap(), b"secret");
        for args in [
            json!({"path":".","max_bytes":0}),
            json!({"path":".","max_bytes":MAX_FILE_BYTES+1}),
            json!({"path":".","extra":true}),
        ] {
            assert!(workspace.perform("ssmcp.fs.read", args).await.is_err());
        }
        assert!(
            workspace
                .perform("ssmcp.fs.write", json!({"path":"bad","data_base64":"%%%"}))
                .await
                .is_err()
        );
        assert!(
            workspace
                .perform(
                    "ssmcp.fs.write",
                    json!({"path":"big","data_base64":STANDARD.encode(vec![0;MAX_FILE_BYTES+1])})
                )
                .await
                .is_err()
        );
        let fifo = CString::new(scratch.0.join("fifo").as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        assert!(
            workspace
                .perform("ssmcp.fs.read", json!({"path":"fifo"}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn directory_pagination_and_change_detection() {
        let scratch = Scratch::new();
        let workspace = scratch.workspace();
        for n in 0..9 {
            std::fs::write(scratch.0.join(format!("{n}")), b"x").unwrap();
        }
        let first = call(&workspace, "ssmcp.fs.list", json!({"max_entries":3})).await;
        assert_eq!(first["entries"].as_array().unwrap().len(), 3);
        let second = call(
            &workspace,
            "ssmcp.fs.list",
            json!({"max_entries":3,"cursor":first["next_cursor"]}),
        )
        .await;
        assert_eq!(second["entries"].as_array().unwrap().len(), 3);
        let mut names = first["entries"]
            .as_array()
            .unwrap()
            .iter()
            .chain(second["entries"].as_array().unwrap())
            .map(|entry| entry["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), 6);
        std::fs::write(scratch.0.join("new"), b"x").unwrap();
        assert!(
            workspace
                .perform("ssmcp.fs.list", json!({"cursor":first["next_cursor"]}))
                .await
                .is_err()
        );
        call(
            &workspace,
            "ssmcp.fs.mkdir",
            json!({"path":"a/b","parents":true}),
        )
        .await;
        assert!(scratch.0.join("a/b").is_dir());
    }
    #[tokio::test]
    async fn tcp_is_duplex_with_half_close_and_distinguishes_timeout_from_eof() {
        let scratch = Scratch::new();
        let workspace = scratch.workspace();
        let listener = call(&workspace, "ssmcp.net.listen", json!({})).await;
        let left = call(
            &workspace,
            "ssmcp.net.connect",
            json!({"port":listener["port"]}),
        )
        .await;
        let right = call(
            &workspace,
            "ssmcp.net.accept",
            json!({"id":listener["id"],"wait_ms":100}),
        )
        .await["stream"]
            .clone();
        assert!(right["id"].is_string());
        let idle = call(
            &workspace,
            "ssmcp.net.read",
            json!({"id":right["id"],"wait_ms":1}),
        )
        .await;
        assert_eq!(idle["timed_out"], true);
        assert_eq!(idle["eof"], false);
        call(
            &workspace,
            "ssmcp.net.write",
            json!({"id":left["id"],"data_base64":"AAEC/w=="}),
        )
        .await;
        let data = call(
            &workspace,
            "ssmcp.net.read",
            json!({"id":right["id"],"wait_ms":100}),
        )
        .await;
        assert_eq!(data["data_base64"], "AAEC/w==");
        call(&workspace, "ssmcp.net.shutdown", json!({"id":left["id"]})).await;
        let eof = call(
            &workspace,
            "ssmcp.net.read",
            json!({"id":right["id"],"wait_ms":100}),
        )
        .await;
        assert_eq!(eof["eof"], true);
        assert_eq!(eof["timed_out"], false);
        call(
            &workspace,
            "ssmcp.net.write",
            json!({"id":right["id"],"data_base64":"cmVwbHk="}),
        )
        .await;
        let reply = call(
            &workspace,
            "ssmcp.net.read",
            json!({"id":left["id"],"wait_ms":100}),
        )
        .await;
        assert_eq!(reply["data_base64"], "cmVwbHk=");
        assert!(
            workspace
                .perform(
                    "ssmcp.net.write",
                    json!({"id":left["id"],"data_base64":"eA=="})
                )
                .await
                .is_err()
        );
        assert_eq!(
            call(&workspace, "ssmcp.net.close", json!({"id":left["id"]})).await["closed"],
            true
        );
        assert_eq!(
            call(&workspace, "ssmcp.net.close", json!({"id":left["id"]})).await["closed"],
            false
        );
        workspace.shutdown().await;
        assert!(
            workspace
                .perform("ssmcp.net.list", json!({}))
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn tcp_limits_loopback_scope_and_close_cancel_waiters() {
        let scratch = Scratch::new();
        let workspace = Arc::new(scratch.workspace());
        for args in [
            json!({"host":"0.0.0.0"}),
            json!({"host":"example.com"}),
            json!({"host":"192.168.0.1"}),
        ] {
            assert!(workspace.perform("ssmcp.net.listen", args).await.is_err());
        }
        let listener = call(&workspace, "ssmcp.net.listen", json!({})).await;
        let pending_workspace = workspace.clone();
        let pending_id = listener["id"].clone();
        let pending = tokio::spawn(async move {
            pending_workspace
                .perform("ssmcp.net.accept", json!({"id":pending_id,"wait_ms":5000}))
                .await
        });
        tokio::task::yield_now().await;
        call(&workspace, "ssmcp.net.close", json!({"id":listener["id"]})).await;
        assert!(
            timeout(Duration::from_secs(1), pending)
                .await
                .unwrap()
                .unwrap()
                .is_err()
        );
        for _ in 0..MAX_HANDLES {
            call(&workspace, "ssmcp.net.listen", json!({})).await;
        }
        assert!(
            workspace
                .perform("ssmcp.net.listen", json!({}))
                .await
                .is_err()
        );
        assert_eq!(
            call(&workspace, "ssmcp.net.list", json!({})).await["handles"]
                .as_array()
                .unwrap()
                .len(),
            MAX_HANDLES
        );
        workspace.shutdown().await;
    }
}
