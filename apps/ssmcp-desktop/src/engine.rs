//! One async connection owner. UI intents never run on the native event thread.
use crate::model::*;
use serde_json::{Value, json};
use ssmcp::session::attach::{EventObserver, RemoteFrontend};
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    sync::{Notify, mpsc},
    time::{Instant, timeout, timeout_at},
};
use zgpui::bridge::{BridgeModel, CommandOutcome, CommandRecord, protocol::*};

const READ_TIMEOUT: Duration = Duration::from_secs(8);
const REFRESH_TIMEOUT: Duration = Duration::from_secs(15);
const COMMAND_TIMEOUT: Duration = Duration::from_secs(12);
const REFRESH_INTERVAL: Duration = Duration::from_secs(5);
const OBSERVATION_MS: u16 = 1500;

pub enum Intent {
    RefreshSessions,
    Connect { target: Target, control: bool },
    Disconnect,
    Refresh,
    SelectThread(ThreadChoice),
    ThreadsPage(usize),
    QueuePage(usize),
    Command { connection: u64, request: Request },
}

#[derive(Default)]
struct Shared {
    view: Mutex<View>,
    revision: AtomicU64,
    stop: AtomicBool,
    stop_notify: Notify,
}

impl Shared {
    fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        // One serial engine owns the only read waiter. notify_one also retains
        // a permit when shutdown lands between the flag check and first poll.
        self.stop_notify.notify_one();
    }

    async fn stopped(&self) {
        loop {
            let notified = self.stop_notify.notified();
            if self.stop.load(Ordering::Acquire) {
                return;
            }
            notified.await;
        }
    }
}

/// One absolute deadline for a complete read/reconciliation pass. Copying the
/// budget into paged reads never renews it. This is not used for sent commands.
#[derive(Clone, Copy)]
struct ReadBudget {
    deadline: Instant,
}

impl ReadBudget {
    fn new(duration: Duration) -> Self {
        Self {
            deadline: Instant::now() + duration,
        }
    }

    fn check(self, shared: &Shared) -> Result<(), BridgeError> {
        if shared.stop.load(Ordering::Acquire) {
            return Err(BridgeError::new(
                ErrorCode::Closed,
                "desktop observation stopped",
            ));
        }
        if Instant::now() >= self.deadline {
            return Err(BridgeError::new(
                ErrorCode::Unavailable,
                "incomplete refresh: shared read budget exhausted",
            ));
        }
        Ok(())
    }

    async fn wait<F: Future>(self, shared: &Shared, future: F) -> Result<F::Output, BridgeError> {
        self.check(shared)?;
        let deadline = self.deadline.min(Instant::now() + READ_TIMEOUT);
        let output = tokio::select! {
            biased;
            _=shared.stopped()=>return Err(BridgeError::new(ErrorCode::Closed,"desktop observation stopped")),
            result=timeout_at(deadline,future)=>result.map_err(|_|BridgeError::new(ErrorCode::Unavailable,"incomplete refresh: read deadline elapsed"))?,
        };
        self.check(shared)?;
        Ok(output)
    }
}

pub struct Handle {
    shared: Arc<Shared>,
    sender: mpsc::Sender<Intent>,
    thread: Option<JoinHandle<()>>,
}

impl Handle {
    pub fn start(target: Option<Target>, control: bool) -> Result<Self, String> {
        let shared = Arc::new(Shared::default());
        let (sender, receiver) = mpsc::channel(16);
        let owned = shared.clone();
        let thread = thread::Builder::new()
            .name("ssmcp-desktop-bridge".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build();
                match runtime {
                    Ok(runtime) => {
                        runtime.block_on(Engine::new(owned, receiver).run(target, control))
                    }
                    Err(_) => {
                        let mut view = owned.view.lock().unwrap_or_else(|error| error.into_inner());
                        view.notice = "无法启动本地异步运行时。".into();
                        owned.revision.fetch_add(1, Ordering::Release);
                    }
                }
            })
            .map_err(|_| "无法启动后台连接线程。".to_owned())?;
        Ok(Self {
            shared,
            sender,
            thread: Some(thread),
        })
    }

    pub fn revision(&self) -> u64 {
        self.shared.revision.load(Ordering::Acquire)
    }

    pub fn snapshot(&self) -> View {
        self.shared
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub fn send(&self, intent: Intent) -> Result<(), &'static str> {
        self.sender
            .try_send(intent)
            .map_err(|_| "操作队列正忙，请等待当前请求结束。")
    }

    pub fn shutdown(&mut self) {
        self.shared.request_stop();
        // Read waits are woken immediately. A command already in flight keeps
        // its own receipt/unknown semantics, then lease release is bounded to 2s.
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shutdown();
    }
}

pub struct RequestIds {
    prefix: String,
    next: u64,
}

impl RequestIds {
    pub fn new(source: &str) -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            prefix: format!("desktop-{source}-{}-{nanos}", std::process::id()),
            next: 0,
        }
    }

    pub fn next(&mut self) -> Id {
        // Exhaustion is unreachable in one process, but it must never wrap to
        // an earlier correlation ID or a chat admission idempotency key.
        self.next = self
            .next
            .checked_add(1)
            .expect("desktop request ID space exhausted");
        Id::new(format!("{}-{}", self.prefix, self.next)).expect("bounded ASCII request ID")
    }
}

struct Engine {
    shared: Arc<Shared>,
    receiver: mpsc::Receiver<Intent>,
    view: View,
    frontend: Option<RemoteFrontend>,
    model: BridgeModel,
    observer: Option<EventObserver>,
    retained_generation: Option<Id>,
    screens: Screens,
    ids: RequestIds,
    last_refresh: Instant,
    refresh_due: bool,
}

impl Engine {
    fn new(shared: Arc<Shared>, receiver: mpsc::Receiver<Intent>) -> Self {
        Self {
            shared,
            receiver,
            view: View::default(),
            frontend: None,
            model: BridgeModel::default(),
            observer: None,
            retained_generation: None,
            screens: Screens::default(),
            ids: RequestIds::new("read"),
            last_refresh: Instant::now(),
            refresh_due: false,
        }
    }

    fn publish(&self) {
        *self
            .shared
            .view
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = self.view.clone();
        self.shared.revision.fetch_add(1, Ordering::Release);
    }

    async fn run(mut self, target: Option<Target>, control: bool) {
        self.refresh_sessions().await;
        if !self.shared.stop.load(Ordering::Acquire) {
            if let Some(target) = target {
                self.connect(target, control).await;
            }
        }
        while !self.shared.stop.load(Ordering::Acquire) {
            if let Ok(intent) = self.receiver.try_recv() {
                self.intent(intent).await;
                continue;
            }
            if self.view.phase.connected() {
                let refresh = (self.refresh_due
                    && self.last_refresh.elapsed() >= Duration::from_millis(300))
                    || self.last_refresh.elapsed() >= REFRESH_INTERVAL;
                if refresh {
                    match self.refresh_resources().await {
                        Ok(()) => {}
                        Err(error) => {
                            self.failed(error).await;
                            continue;
                        }
                    }
                    if self.shared.stop.load(Ordering::Acquire) || !self.receiver.is_empty() {
                        continue;
                    }
                }
                if self.view.conversation_more && self.view.resource("conversation") {
                    if let Err(error) = self
                        .refresh_conversation(4, ReadBudget::new(REFRESH_TIMEOUT))
                        .await
                    {
                        self.failed(error).await;
                        continue;
                    }
                    self.publish();
                    if self.shared.stop.load(Ordering::Acquire) || !self.receiver.is_empty() {
                        continue;
                    }
                    // Catch up retained history in bounded pages while yielding
                    // to queued user actions between each batch.
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                if self.model.capabilities().is_some_and(|caps| caps.events) {
                    if let Err(error) = self.observe().await {
                        self.failed(error).await;
                    }
                } else {
                    self.wait_intent().await;
                }
            } else {
                self.wait_intent().await;
            }
        }
        self.view.phase = Phase::Closing;
        self.publish();
        self.release().await;
    }

    async fn wait_intent(&mut self) {
        if let Ok(Some(intent)) = ReadBudget::new(Duration::from_millis(200))
            .wait(&self.shared, self.receiver.recv())
            .await
        {
            self.intent(intent).await;
        }
    }

    async fn intent(&mut self, intent: Intent) {
        match intent {
            Intent::RefreshSessions => self.refresh_sessions().await,
            Intent::Connect { target, control } => self.connect(target, control).await,
            Intent::Disconnect => {
                let released = self.release().await;
                self.view.phase = Phase::Disconnected;
                self.view.clear_live(false);
                self.view.notice = if released {
                    "已确认释放桌面连接；CLI 会话继续运行。"
                } else {
                    "桌面连接已关闭，租约释放尚未确认；CLI 会话继续运行，租约由服务按期处理。"
                }
                .into();
                self.publish();
            }
            Intent::Refresh => {
                if self.view.phase.connected() {
                    self.refresh_due = true;
                }
            }
            Intent::SelectThread(thread) => {
                if self.view.resource("conversation") {
                    self.view.thread = thread;
                    self.view.conversation = History::default();
                    self.view.conversation_cursor = None;
                    self.view.conversation_more = false;
                    self.refresh_due = true;
                    self.publish();
                }
            }
            Intent::ThreadsPage(offset) => {
                if offset <= 1024 && self.view.resource("threads") {
                    self.view.threads_offset = offset;
                    self.refresh_due = true;
                }
            }
            Intent::QueuePage(offset) => {
                if offset <= 4096 && self.view.resource("chat.queue") {
                    self.view.queue_offset = offset;
                    self.refresh_due = true;
                }
            }
            Intent::Command {
                connection,
                request,
            } => self.command(connection, request).await,
        }
    }

    async fn refresh_sessions(&mut self) {
        self.view.sessions_loading = true;
        self.publish();
        match ReadBudget::new(READ_TIMEOUT)
            .wait(&self.shared, ssmcp::session::list())
            .await
        {
            Ok(Ok(values)) => self.view.sessions = sessions(values),
            Ok(Err(_)) | Err(_) => {
                self.view.notice = "无法列出本地会话。请用 CLI 检查当前用户的会话服务。".into()
            }
        }
        self.view.sessions_loading = false;
        self.publish();
    }

    async fn release(&mut self) -> bool {
        let released = if let Some(mut frontend) = self.frontend.take() {
            matches!(
                timeout(Duration::from_secs(2), frontend.close()).await,
                Ok(Ok(()))
            )
        } else {
            true
        };
        self.model.disconnect();
        released
    }

    async fn connect(&mut self, target: Target, control: bool) {
        let budget = ReadBudget::new(REFRESH_TIMEOUT);
        let same_target = self.view.target.as_ref() == Some(&target);
        self.release().await;
        self.view.connection = self
            .view
            .connection
            .checked_add(1)
            .expect("desktop connection ID space exhausted");
        self.view.phase = Phase::Connecting;
        self.view.busy = false;
        self.view.notice.clear();
        self.view.clear_live(!same_target);
        self.view.target = Some(target.clone());
        if !same_target {
            self.observer = None;
            self.retained_generation = None;
            self.screens = Screens::default();
            self.view.thread = ThreadChoice::All;
            self.view.threads_offset = 0;
            self.view.queue_offset = 0;
        }
        self.publish();
        let opened = budget
            .wait(&self.shared, async {
                match &target {
                    Target::Named(name) => RemoteFrontend::connect_named(name, !control).await,
                    Target::Directory(directory) => {
                        RemoteFrontend::connect(directory.clone(), !control).await
                    }
                }
            })
            .await;
        match opened {
            Ok(Ok(frontend)) => self.frontend = Some(frontend),
            _ => {
                self.view.phase = Phase::Disconnected;
                self.view.notice = if control {
                    "未获得控制连接。请确认会话在线，且没有其它界面占用输入租约。"
                } else {
                    "无法连接本地会话。请确认 CLI 会话已经运行，且属于当前用户。"
                }
                .into();
                self.publish();
                return;
            }
        }
        let request = Request {
            version: VERSION,
            id: self.ids.next(),
            operation: Operation::Capabilities {},
        };
        let result = self.read_exchange(request, budget).await;
        let capabilities = match result {
            Ok(ResponseBody::Capabilities(capabilities)) => capabilities,
            Ok(_) => {
                self.failed(BridgeError::invalid("missing capabilities"))
                    .await;
                return;
            }
            Err(error) => {
                self.failed(error).await;
                return;
            }
        };
        let Some(generation) = self.model.generation().cloned() else {
            self.failed(BridgeError::invalid("missing generation"))
                .await;
            return;
        };
        if self.retained_generation.as_ref() != Some(&generation) {
            self.view.clear_live(true);
            self.screens = Screens::default();
            self.observer = Some(EventObserver::new(generation.clone(), None));
            if same_target && self.retained_generation.is_some() {
                self.view.notice =
                    "后端已进入新代次；已刷新当前状态，旧命令回执仍保留原代次。".into();
            }
        }
        self.retained_generation = Some(generation.clone());
        self.view.generation = Some(generation.clone());
        self.view.phase = if capabilities.mode == Mode::Control {
            Phase::Controlling
        } else {
            Phase::Observing
        };
        self.view.capabilities = Some(capabilities);
        // Establish the first live boundary before reading state snapshots.
        // Existing observers keep their previous confirmed cursor on reconnect.
        if self.observer.is_none() {
            self.observer = Some(EventObserver::new(generation, None));
        }
        if self.model.capabilities().is_some_and(|caps| caps.events)
            && self
                .observer
                .as_ref()
                .is_some_and(|observer| observer.cursor().is_none())
        {
            if let Err(error) = self.read_initial_boundary(budget).await {
                self.failed(error).await;
                return;
            }
        }
        match self.refresh_resources_with(budget).await {
            Ok(()) => self.publish(),
            Err(error) => self.failed(error).await,
        }
    }

    async fn exchange_command(&mut self, request: Request) -> Result<ResponseBody, BridgeError> {
        if !request.operation.is_command() {
            return Err(BridgeError::invalid("expected command exchange"));
        }
        let frontend = self
            .frontend
            .as_ref()
            .ok_or_else(|| BridgeError::new(ErrorCode::Closed, "not connected"))?;
        // Stop notifications deliberately do not race an already sent command.
        // Timeout still drops Flight and records its original outcome as unknown.
        let result = timeout(
            COMMAND_TIMEOUT,
            self.model
                .request_with(request, |request| frontend.request(request)),
        )
        .await;
        match result {
            Ok(result) => result,
            Err(_) => Err(self.model.last_error().cloned().unwrap_or_else(|| {
                BridgeError::new(ErrorCode::Unavailable, "request deadline elapsed")
            })),
        }
    }

    async fn read_exchange(
        &mut self,
        request: Request,
        budget: ReadBudget,
    ) -> Result<ResponseBody, BridgeError> {
        if request.operation.is_command() {
            return Err(BridgeError::invalid(
                "commands cannot use a cancellable read budget",
            ));
        }
        let frontend = self
            .frontend
            .as_ref()
            .ok_or_else(|| BridgeError::new(ErrorCode::Closed, "not connected"))?;
        budget
            .wait(
                &self.shared,
                self.model
                    .request_with(request, |request| frontend.request(request)),
            )
            .await?
    }

    async fn state(
        &mut self,
        resource: &str,
        query: Value,
        budget: ReadBudget,
    ) -> Result<Value, BridgeError> {
        let generation = self
            .model
            .generation()
            .cloned()
            .ok_or_else(|| BridgeError::new(ErrorCode::Closed, "not connected"))?;
        let request = Request {
            version: VERSION,
            id: self.ids.next(),
            operation: Operation::State {
                generation,
                resource: Id::new(resource)?,
                query,
            },
        };
        match self.read_exchange(request, budget).await? {
            ResponseBody::State(state) => Ok(state.value),
            _ => Err(BridgeError::invalid("unexpected state response")),
        }
    }

    async fn refresh_resources(&mut self) -> Result<(), BridgeError> {
        self.refresh_resources_with(ReadBudget::new(REFRESH_TIMEOUT))
            .await
    }

    async fn refresh_resources_with(&mut self, budget: ReadBudget) -> Result<(), BridgeError> {
        // Readiness and advertised commands can change without a generation
        // change. A refresh must not keep an obsolete control capability set.
        let request = Request {
            version: VERSION,
            id: self.ids.next(),
            operation: Operation::Capabilities {},
        };
        let capabilities = match self.read_exchange(request, budget).await? {
            ResponseBody::Capabilities(capabilities) => capabilities,
            _ => return Err(BridgeError::invalid("unexpected capability response")),
        };
        let generation = self
            .model
            .generation()
            .cloned()
            .ok_or_else(|| BridgeError::invalid("missing generation"))?;
        if self.retained_generation.as_ref() != Some(&generation) {
            self.view.clear_live(true);
            self.screens = Screens::default();
            self.observer = Some(EventObserver::new(generation.clone(), None));
            self.retained_generation = Some(generation.clone());
            self.view.notice =
                "后端代次已改变。已建立新的观察边界；旧命令结果保留在原代次中。".into();
        }
        self.view.generation = Some(generation);
        self.view.phase = if capabilities.mode == Mode::Control {
            Phase::Controlling
        } else {
            Phase::Observing
        };
        self.view.capabilities = Some(capabilities);
        if !self.view.resource("terminals") {
            self.view.terminals.clear();
            self.screens = Screens::default();
        }
        if !self.view.resource("workers") {
            self.view.workers = None;
        }
        if !self.view.resource("time") {
            self.view.time = None;
        }
        if !self.view.resource("threads") {
            self.view.agents.clear();
            self.view.can_send = false;
        }
        if !self.view.resource("chat.queue") {
            self.view.queue.clear();
        }
        if self.model.capabilities().is_some_and(|caps| caps.events)
            && self
                .observer
                .as_ref()
                .is_some_and(|observer| observer.cursor().is_none())
        {
            self.read_initial_boundary(budget).await?;
        }
        if self.view.resource("runtime") {
            let runtime = self.state("runtime", json!({}), budget).await?;
            self.view.backend_active = runtime["backend_active"] == true;
        }
        if self.view.resource("terminals") {
            let value = self.state("terminals", json!({}), budget).await?;
            self.view.terminals = self.screens.sync(&value, &self.view.terminals);
        }
        if self.view.resource("workers") {
            self.refresh_workers(budget).await?;
        }
        if self.view.resource("time") {
            let value = self.state("time", json!({}), budget).await?;
            self.view.time = Some(WaitSnapshot::parse(&value)?);
        }
        if self.view.resource("threads") {
            self.refresh_threads(budget).await?;
        }
        if self.view.resource("chat.queue") {
            self.refresh_queue(budget).await?;
        }
        if self.view.resource("conversation") {
            self.refresh_conversation(4, budget).await?;
        }
        // A partially reconciled pass must not publish a complete/fresh state.
        // Already validated conversation pages retain their own safe cursors.
        budget.check(&self.shared)?;
        self.last_refresh = Instant::now();
        self.refresh_due = false;
        self.publish();
        Ok(())
    }

    async fn refresh_threads(&mut self, budget: ReadBudget) -> Result<(), BridgeError> {
        let value = self
            .state(
                "threads",
                json!({"offset":self.view.threads_offset,"limit":32}),
                budget,
            )
            .await?;
        self.view.agents = value["agents"]
            .as_array()
            .into_iter()
            .flatten()
            .take(32)
            .filter_map(|value| {
                let id = value["agent_id"]
                    .as_str()
                    .filter(|id| !id.is_empty() && id.len() <= 128)?;
                Some(Agent {
                    id: id.into(),
                    name: text(value, "name", 128),
                    role: text(value, "role", 64),
                    status: text(value, "status", 64),
                })
            })
            .collect();
        self.view.threads_next = next_offset(&value, self.view.threads_offset, 1024)?;
        self.view.can_send = value["can_send"] == true;
        self.view.reloading = value["reloading"] == true;
        Ok(())
    }

    async fn refresh_queue(&mut self, budget: ReadBudget) -> Result<(), BridgeError> {
        let value = self
            .state(
                "chat.queue",
                json!({"offset":self.view.queue_offset,"limit":16}),
                budget,
            )
            .await?;
        self.view.queue = value["items"]
            .as_array()
            .into_iter()
            .flatten()
            .take(16)
            .map(|value| QueueItem {
                id: text(value, "id", 128),
                text: text(value, "text", 2048),
                target: text(value, "target", 128),
                status: text(value, "status", 64),
                uncertain: value["uncertain"] == true,
                retarget_required: value["retarget_required"] == true,
            })
            .collect();
        self.view.queue_next = next_offset(&value, self.view.queue_offset, 4096)?;
        self.view.queue_more = value["has_more"] == true;
        Ok(())
    }

    async fn refresh_conversation(
        &mut self,
        pages: usize,
        budget: ReadBudget,
    ) -> Result<(), BridgeError> {
        for _ in 0..pages {
            let query = self.view.thread.query(self.view.conversation_cursor);
            let value = self.state("conversation", query, budget).await?;
            let (rows, cursor, more, gap) =
                conversation_page(&value, self.view.conversation_cursor)?;
            if gap {
                self.view.gaps = self.view.gaps.saturating_add(1);
                self.view.notice =
                    "部分历史已超出 CLI 的内存保留窗口；这里只显示实际仍可读取的记录。".into();
            }
            for row in rows {
                self.view.conversation.push(row);
            }
            self.view.conversation_cursor = Some(cursor);
            self.view.conversation_more = more;
            if !more {
                break;
            }
        }
        Ok(())
    }

    async fn refresh_workers(&mut self, budget: ReadBudget) -> Result<(), BridgeError> {
        let mut offset = 0;
        let mut workers: Option<Workers> = None;
        let mut slot_count = None;
        loop {
            let value = self
                .state("workers", json!({"offset":offset,"limit":32}), budget)
                .await?;
            let observed_slot_count = value["slot_count"]
                .as_u64()
                .filter(|count| *count <= 128)
                .ok_or_else(|| BridgeError::invalid("invalid worker slot count"))?
                as usize;
            if slot_count.is_some_and(|expected| expected != observed_slot_count) {
                return Err(BridgeError::new(
                    ErrorCode::Conflict,
                    "worker capacity changed while paging",
                ));
            }
            slot_count = Some(observed_slot_count);
            let generation = text(&value, "pool_generation", 128);
            if generation.is_empty() {
                return Err(BridgeError::invalid("missing worker pool generation"));
            }
            if let Some(workers) = workers.as_ref() {
                if workers.generation != generation {
                    return Err(BridgeError::new(
                        ErrorCode::Conflict,
                        "worker pool changed while paging",
                    ));
                }
            } else {
                workers = Some(Workers {
                    generation,
                    total: count(&value, "total"),
                    desired: count(&value, "desired_total"),
                    registered: count(&value, "registered_workers"),
                    working: count(&value, "working"),
                    idle: count(&value, "idle"),
                    offline: count(&value, "offline"),
                    retiring: count(&value, "retiring"),
                    queued: count(&value, "queued"),
                    closed: value["closed"] == true,
                    stopping: value["stopping"] == true,
                    slots: Vec::new(),
                });
            }
            let target = workers.as_mut().expect("worker summary initialized");
            let slots = value["slots"]
                .as_array()
                .filter(|slots| slots.len() <= 32)
                .ok_or_else(|| BridgeError::invalid("invalid worker page"))?;
            for value in slots {
                let slot = value["slot"]
                    .as_u64()
                    .ok_or_else(|| BridgeError::invalid("invalid worker slot"))?;
                let epoch = counter(&value["epoch"])?;
                if target.slots.iter().any(|worker| worker.slot == slot) {
                    return Err(BridgeError::invalid("duplicate worker slot"));
                }
                let task = text(value, "task_summary", 1024);
                target.slots.push(Worker {
                    slot,
                    epoch: epoch.0.to_string(),
                    state: text(value, "state", 64),
                    registered: value["registered"] == true,
                    task: if task.is_empty() {
                        text(value, "task_id", 128)
                    } else {
                        task
                    },
                    progress: text(value, "progress", 512),
                    elapsed: if value["current_task_elapsed_ms"].is_null() {
                        String::new()
                    } else {
                        format!("{} ms", counter(&value["current_task_elapsed_ms"])?.0)
                    },
                    retiring: value["retire_requested"] == true,
                });
            }
            let Some(next) = next_offset(&value, offset, 128)? else {
                break;
            };
            offset = next;
        }
        if workers
            .as_ref()
            .is_some_and(|workers| Some(workers.slots.len()) != slot_count)
        {
            return Err(BridgeError::new(
                ErrorCode::Conflict,
                "worker page set was incomplete",
            ));
        }
        self.view.workers = workers;
        Ok(())
    }

    async fn read_initial_boundary(&mut self, budget: ReadBudget) -> Result<(), BridgeError> {
        let frontend = self
            .frontend
            .as_ref()
            .ok_or_else(|| BridgeError::new(ErrorCode::Closed, "not connected"))?;
        let observer = self
            .observer
            .as_mut()
            .ok_or_else(|| BridgeError::invalid("missing observer"))?;
        budget
            .wait(
                &self.shared,
                observer.wait_with(self.ids.next(), 64, 0, |request| frontend.request(request)),
            )
            .await??;
        // Leave a nonempty/gap page pending until normal observation applies
        // all its events after state reconciliation. Establishing a live
        // boundary alone is not proof that a returned page was consumed.
        self.view.cursor = observer.cursor();
        Ok(())
    }

    async fn observe(&mut self) -> Result<(), BridgeError> {
        let budget = ReadBudget::new(REFRESH_TIMEOUT);
        budget.check(&self.shared)?;
        let page_generation = self
            .observer
            .as_ref()
            .map(|observer| observer.generation().clone())
            .ok_or_else(|| BridgeError::invalid("missing observer"))?;
        let page = if let Some(page) = self.observer.as_ref().and_then(EventObserver::pending_page)
        {
            if page.retention_gap || page.has_more || !page.events.is_empty() {
                Some(page.clone())
            } else {
                None
            }
        } else {
            None
        };
        let page = match page {
            Some(page) => page,
            None => {
                let frontend = self
                    .frontend
                    .as_ref()
                    .ok_or_else(|| BridgeError::new(ErrorCode::Closed, "not connected"))?;
                let observer = self
                    .observer
                    .as_mut()
                    .ok_or_else(|| BridgeError::invalid("missing observer"))?;
                let page = budget
                    .wait(
                        &self.shared,
                        observer.wait_with(self.ids.next(), 64, OBSERVATION_MS, |request| {
                            frontend.request(request)
                        }),
                    )
                    .await??;
                page.clone()
            }
        };
        if let Some(capabilities) = self.model.capabilities() {
            encoded_len(&page, capabilities.max_event_page_bytes as usize)?;
        }
        if page.retention_gap {
            self.screens.reset(&mut self.view.terminals);
            // Do not commit the gap page if current state cannot be reconciled.
            self.refresh_resources_with(budget).await?;
            if self.view.generation.as_ref() != Some(&page_generation) {
                // Refresh established a new observer. The old page can never
                // be projected into, or advance, that new generation.
                return Ok(());
            }
            self.view.gaps = self.view.gaps.saturating_add(1);
            self.view.notice =
                "事件保留窗口出现缺口。已刷新当前状态；终端画面从可见输出重新建立。".into();
        }
        let unknown_terminal = page.events.iter().any(|event| {
            event.value["kind"] == "terminal"
                && event.value["terminal_id"]
                    .as_str()
                    .is_some_and(|id| !self.view.terminals.iter().any(|terminal| terminal.id == id))
        });
        if unknown_terminal && self.view.resource("terminals") {
            // Discover a newly opened terminal before committing the first
            // output page that mentions it; otherwise its initial bytes vanish.
            let value = self.state("terminals", json!({}), budget).await?;
            self.view.terminals = self.screens.sync(&value, &self.view.terminals);
        }
        budget.check(&self.shared)?;
        for event in &page.events {
            Screens::validate(event, &self.view.terminals)?;
        }
        for event in &page.events {
            self.screens.apply(event, &mut self.view.terminals)?;
            if let Some(row) = event_row(event) {
                self.view.timeline.push(row);
            }
        }
        self.view.cursor = self.observer.as_mut().and_then(EventObserver::advance);
        if !page.events.is_empty() || page.retention_gap {
            self.refresh_due = true;
            self.publish();
        }
        Ok(())
    }

    async fn command(&mut self, connection: u64, request: Request) {
        let Operation::Command {
            generation,
            command,
            arguments,
        } = &request.operation
        else {
            return;
        };
        let arguments = arguments.clone();
        if !self.view.receipt_capacity() {
            self.view.notice = "回执容量已满，新的操作未发送。请先核对未解决的结果。".into();
            self.publish();
            return;
        }
        let mut record = CommandRecord {
            request_id: request.id.clone(),
            generation: generation.clone(),
            command: command.clone(),
            outcome: CommandOutcome::AwaitingReply,
        };
        if self.shared.stop.load(Ordering::Acquire)
            || connection != self.view.connection
            || !self.view.command(command.as_str())
            || self.model.generation() != Some(generation)
        {
            record.outcome = CommandOutcome::Error(BridgeError::new(
                ErrorCode::Denied,
                "command context changed before dispatch",
            ));
            self.view
                .record(Receipt::from_record(&record, connection, &arguments));
            self.publish();
            return;
        }
        self.view
            .record(Receipt::from_record(&record, connection, &arguments));
        self.view.busy = true;
        self.publish();
        let result = self.exchange_command(request).await;
        if let Ok(ResponseBody::Command(receipt)) = &result {
            if receipt.command.as_str() == "terminal.resize" {
                let size = receipt.value["rows"]
                    .as_u64()
                    .zip(receipt.value["cols"].as_u64())
                    .filter(|(rows, cols)| {
                        *rows > 0 && *rows <= 1000 && *cols > 0 && *cols <= 1000
                    });
                if let Some((rows, cols)) = size {
                    self.screens
                        .resize(&mut self.view.terminals, rows as u16, cols as u16);
                }
            }
        }
        if let Some(actual) = self.model.last_command().filter(|actual| {
            actual.request_id == record.request_id
                && actual.generation == record.generation
                && actual.command == record.command
        }) {
            record = actual.clone();
        } else if let Err(error) = &result {
            // Preflight failure: no transport callback ran, so no effect occurred.
            record.outcome = CommandOutcome::Error(error.clone());
        }
        self.view
            .record(Receipt::from_record(&record, connection, &arguments));
        self.view.busy = false;
        self.refresh_due = true;
        if let Err(error) = result {
            self.view.notice = if error.effect == Effect::MayHaveOccurred {
                "命令结果未知。保留了原请求和代次；请查看当前状态，客户端不会重发。".into()
            } else {
                error_label(error.code).into()
            };
            if self.model.generation().is_none()
                || matches!(
                    error.code,
                    ErrorCode::Closed | ErrorCode::Denied | ErrorCode::StaleGeneration
                )
            {
                self.release().await;
                self.view.phase = Phase::Disconnected;
                self.view.clear_live(false);
            }
        }
        self.publish();
    }

    async fn failed(&mut self, error: BridgeError) {
        self.view.notice = error_label(error.code).into();
        self.release().await;
        self.view.phase = Phase::Disconnected;
        self.view.busy = false;
        self.view.clear_live(false);
        self.publish();
    }
}

fn count(value: &Value, key: &str) -> u64 {
    value[key].as_u64().unwrap_or(0)
}

fn next_offset(
    value: &Value,
    previous: usize,
    maximum: usize,
) -> Result<Option<usize>, BridgeError> {
    // Chat pages return their final end offset even when has_more is false;
    // worker pages instead use a null next_offset at the end.
    if value["has_more"] == false {
        return Ok(None);
    }
    if value["has_more"] == true && value["next_offset"].is_null() {
        return Err(BridgeError::invalid("missing continuation offset"));
    }
    if value["next_offset"].is_null() {
        return Ok(None);
    }
    let next = value["next_offset"]
        .as_u64()
        .and_then(|value| usize::try_from(value).ok())
        .filter(|next| *next > previous && *next < maximum)
        .ok_or_else(|| BridgeError::invalid("nonmonotonic page offset"))?;
    Ok(Some(next))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pagination_cannot_loop_or_exceed_capacity() {
        assert!(next_offset(&json!({"next_offset":32}), 32, 128).is_err());
        assert!(next_offset(&json!({"next_offset":128}), 96, 128).is_err());
        assert_eq!(
            next_offset(&json!({"next_offset":64}), 32, 128).unwrap(),
            Some(64)
        );
        assert_eq!(
            next_offset(&json!({"next_offset":0,"has_more":false}), 0, 128).unwrap(),
            None
        );
    }

    #[test]
    fn request_ids_leave_room_for_observer_round_suffix() {
        let mut ids = RequestIds::new("read");
        let first = ids.next();
        let second = ids.next();
        assert_ne!(first, second);
        assert!(first.as_str().len() < 100);
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn paged_reads_share_one_absolute_budget() {
        let shared = Shared::default();
        let started = Instant::now();
        let budget = ReadBudget::new(REFRESH_TIMEOUT);
        budget
            .wait(&shared, tokio::time::sleep(Duration::from_secs(6)))
            .await
            .unwrap();
        budget
            .wait(&shared, tokio::time::sleep(Duration::from_secs(6)))
            .await
            .unwrap();
        let error = budget
            .wait(&shared, tokio::time::sleep(Duration::from_secs(6)))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Unavailable);
        assert_eq!(error.effect, Effect::None);
        assert_eq!(started.elapsed(), REFRESH_TIMEOUT);
        assert!(budget.check(&shared).is_err());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn shutdown_wakes_and_drops_a_pending_read_without_waiting_for_its_deadline() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let shared = Arc::new(Shared::default());
        let dropped = Arc::new(AtomicBool::new(false));
        let worker_shared = shared.clone();
        let worker_dropped = dropped.clone();
        let (entered, started_read) = tokio::sync::oneshot::channel();
        let started = Instant::now();
        let read = tokio::spawn(async move {
            ReadBudget::new(REFRESH_TIMEOUT)
                .wait(&worker_shared, async move {
                    let _guard = Dropped(worker_dropped);
                    let _ = entered.send(());
                    std::future::pending::<()>().await;
                })
                .await
        });
        started_read.await.unwrap();
        shared.request_stop();
        let error = read.await.unwrap().unwrap_err();
        assert_eq!(error.code, ErrorCode::Closed);
        assert!(dropped.load(Ordering::Acquire));
        assert_eq!(started.elapsed(), Duration::ZERO);
        // The stop-before-first-poll race must not start one last IPC request.
        let polled = std::cell::Cell::new(false);
        assert!(
            ReadBudget::new(REFRESH_TIMEOUT)
                .wait(&shared, async {
                    polled.set(true);
                })
                .await
                .is_err()
        );
        assert!(!polled.get());
    }

    #[tokio::test(flavor = "current_thread", start_paused = true)]
    async fn stopped_event_read_preserves_its_last_validated_cursor() {
        let shared = Arc::new(Shared::default());
        let stopper = shared.clone();
        let stop = tokio::spawn(async move {
            tokio::task::yield_now().await;
            stopper.request_stop();
        });
        let cursor = Counter(9_007_199_254_740_993);
        let mut observer = EventObserver::new(Id::new("generation-one").unwrap(), Some(cursor));
        let calls = std::cell::Cell::new(0);
        let result = ReadBudget::new(REFRESH_TIMEOUT)
            .wait(
                &shared,
                observer.wait_with(
                    Id::new("observation-one").unwrap(),
                    64,
                    OBSERVATION_MS,
                    |_| {
                        calls.set(calls.get() + 1);
                        std::future::pending::<Result<Response, BridgeError>>()
                    },
                ),
            )
            .await;
        assert_eq!(result.unwrap_err().code, ErrorCode::Closed);
        assert_eq!(observer.cursor(), Some(cursor));
        assert!(observer.pending_page().is_none());
        assert_eq!(calls.get(), 1);
        stop.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn commands_cannot_accidentally_use_the_interruptible_read_path() {
        let (_, receiver) = mpsc::channel(1);
        let mut engine = Engine::new(Arc::new(Shared::default()), receiver);
        let request = Request {
            version: VERSION,
            id: Id::new("action-one").unwrap(),
            operation: Operation::Command {
                generation: Id::new("generation-one").unwrap(),
                command: Id::new("terminal.write").unwrap(),
                arguments: json!({"data":"x"}),
            },
        };
        let error = engine
            .read_exchange(request, ReadBudget::new(REFRESH_TIMEOUT))
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        assert!(engine.model.last_command().is_none());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn command_timeout_keeps_original_unknown_outcome_after_rediscovery() {
        use std::cell::Cell;
        let mut model = BridgeModel::default();
        let capabilities = Capabilities {
            application: Id::new("ssmcp").unwrap(),
            mode: Mode::Control,
            resources: vec![],
            commands: vec![Command {
                id: Id::new("chat.send").unwrap(),
                description: "explicit input".into(),
                input_schema: json!({"type":"object"}),
            }],
            events: false,
            max_frame_bytes: MAX_FRAME_BYTES as u32,
            max_event_page_bytes: MAX_PAGE_BYTES as u32,
        };
        let discover = |name: &str| Request {
            version: VERSION,
            id: Id::new(name).unwrap(),
            operation: Operation::Capabilities {},
        };
        model
            .request_with(discover("discover-one"), |request| async {
                Ok(Response {
                    version: VERSION,
                    id: request.id,
                    generation: Some(Id::new("old-generation").unwrap()),
                    body: ResponseBody::Capabilities(capabilities.clone()),
                })
            })
            .await
            .unwrap();
        let calls = Cell::new(0);
        let request = Request {
            version: VERSION,
            id: Id::new("original-action").unwrap(),
            operation: Operation::Command {
                generation: Id::new("old-generation").unwrap(),
                command: Id::new("chat.send").unwrap(),
                arguments: json!({"text":"one input"}),
            },
        };
        let expired = timeout(
            Duration::from_millis(2),
            model.request_with(request, |_| {
                calls.set(calls.get() + 1);
                std::future::pending::<Result<Response, BridgeError>>()
            }),
        )
        .await;
        assert!(expired.is_err());
        assert!(model.last_command().unwrap().is_uncertain());
        model
            .request_with(discover("discover-two"), |request| async {
                Ok(Response {
                    version: VERSION,
                    id: request.id,
                    generation: Some(Id::new("new-generation").unwrap()),
                    body: ResponseBody::Capabilities(capabilities.clone()),
                })
            })
            .await
            .unwrap();
        assert_eq!(calls.get(), 1);
        let original = model.last_command().unwrap();
        assert_eq!(original.generation.as_str(), "old-generation");
        assert_eq!(original.request_id.as_str(), "original-action");
        assert!(original.is_uncertain());
    }
    #[tokio::test(flavor = "current_thread")]
    async fn queued_command_cannot_move_to_another_connection_with_the_same_generation() {
        let (_, receiver) = mpsc::channel(1);
        let mut engine = Engine::new(Arc::new(Shared::default()), receiver);
        let capabilities = Capabilities {
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
        };
        let generation = Id::new("same-generation").unwrap();
        engine
            .model
            .request_with(
                Request {
                    version: VERSION,
                    id: Id::new("discover").unwrap(),
                    operation: Operation::Capabilities {},
                },
                |request| async {
                    Ok(Response {
                        version: VERSION,
                        id: request.id,
                        generation: Some(generation.clone()),
                        body: ResponseBody::Capabilities(capabilities.clone()),
                    })
                },
            )
            .await
            .unwrap();
        engine.view.connection = 2;
        engine.view.phase = Phase::Controlling;
        engine.view.generation = Some(generation.clone());
        engine.view.capabilities = Some(capabilities);
        engine
            .command(
                1,
                Request {
                    version: VERSION,
                    id: Id::new("old-connection-action").unwrap(),
                    operation: Operation::Command {
                        generation,
                        command: Id::new("terminal.write").unwrap(),
                        arguments: json!({"terminal_id":"terminal-one","data":"x"}),
                    },
                },
            )
            .await;
        assert!(engine.model.last_command().is_none());
        assert_eq!(engine.view.receipts[0].connection, 1);
        assert_eq!(engine.view.receipts[0].state, ReceiptState::Rejected);
        assert_eq!(
            engine.view.receipts[0].detail,
            error_label(ErrorCode::Denied)
        );
    }
}
