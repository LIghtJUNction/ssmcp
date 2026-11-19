use super::*;
use serde_json::json;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    calls: AtomicUsize,
    active: AtomicUsize,
    cancelled: AtomicUsize,
    cleanups: AtomicUsize,
    entered: Notify,
    release: Notify,
    dropped: Notify,
    cancelled_scope: Mutex<Option<(Option<String>, Option<u64>)>>,
    threads: Mutex<HashSet<std::thread::ThreadId>>,
}

impl Fixture {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
            cleanups: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Notify::new(),
            dropped: Notify::new(),
            cancelled_scope: Mutex::new(None),
            threads: Mutex::new(HashSet::new()),
        })
    }
}

struct RunningCall {
    plugin: Arc<Fixture>,
    completed: bool,
}
impl Drop for RunningCall {
    fn drop(&mut self) {
        self.plugin.active.fetch_sub(1, Ordering::SeqCst);
        if !self.completed {
            self.plugin.cancelled.fetch_add(1, Ordering::SeqCst);
            *self.plugin.cancelled_scope.lock().unwrap() = Some((
                crate::observability::REQUEST_ID.try_with(Clone::clone).ok(),
                HarnessCall::current_id(),
            ));
        }
        self.plugin.dropped.notify_one();
    }
}

impl NativePlugin for Fixture {
    const ID: &'static str = "native-host-fixture";
    fn providers(&self) -> &'static [Provider] {
        &[Provider::Workspace]
    }
    fn catalog(&self) -> Vec<(Provider, Vec<Value>)> {
        vec![(
            Provider::Workspace,
            vec![json!({
                "name":"ssmcp.fs.fixture", "description":"native ABI contract fixture",
                "inputSchema":{"type":"object"},
                "annotations":{"readOnlyHint":true},"x-native-test":{"untouched":true}
            })],
        )]
    }
    fn call(self: Arc<Self>, call: Call) -> PluginFuture {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.active.fetch_add(1, Ordering::SeqCst);
            let mut running = RunningCall {
                plugin: self.clone(),
                completed: false,
            };
            self.threads
                .lock()
                .unwrap()
                .insert(std::thread::current().id());
            if call.arguments["hold"] == true {
                self.entered.notify_one();
                self.release.notified().await;
            }
            if call.arguments["error"] == true {
                running.completed = true;
                return Err("exact native business error".into());
            }
            running.completed = true;
            if let Some(value) = call.arguments.get("raw_result") {
                return Ok(value.clone());
            }
            Ok(json!({"content":[],"structuredContent":{
                "arguments":call.arguments, "native_name":call.native_name,
                "request_id":crate::observability::REQUEST_ID.try_with(Clone::clone).ok(),
                "harness_call":HarnessCall::current_id(),
            },"_meta":{"vendor":{"opaque":true}}}))
        })
    }
    fn shutdown(self: Arc<Self>) -> PluginFuture {
        Box::pin(async move {
            if self.active.load(Ordering::SeqCst) != 0 {
                return Err("plugin cleanup began before active calls were dropped".into());
            }
            self.cleanups.fetch_add(1, Ordering::SeqCst);
            Ok(Value::Null)
        })
    }
}

struct Second(Arc<Fixture>);
impl NativePlugin for Second {
    const ID: &'static str = "second-native-plugin";
    fn providers(&self) -> &'static [Provider] {
        // One declared provider stays unregistered, as with optional harnesses.
        &[Provider::Context, Provider::Rsync]
    }
    fn catalog(&self) -> Vec<(Provider, Vec<Value>)> {
        vec![(
            Provider::Context,
            vec![json!({
                "name":"ssmcp.context.fixture", "inputSchema":{"type":"object"}
            })],
        )]
    }
    fn call(self: Arc<Self>, call: Call) -> PluginFuture {
        self.0.clone().call(call)
    }
    fn shutdown(self: Arc<Self>) -> PluginFuture {
        self.0.clone().shutdown()
    }
}

fn fixture() -> (Arc<NativeHost>, Arc<Fixture>, RegisteredTool) {
    let registry = Registry::new();
    let plugin = Fixture::new();
    let host =
        Arc::new(NativeHost::new(registry.clone(), vec![Definition::new(plugin.clone())]).unwrap());
    for (provider, tools) in host.catalogs().unwrap() {
        registry.replace(provider, tools).unwrap();
    }
    let tool = registry.lookup("ssmcp.fs.fixture").unwrap();
    (host, plugin, tool)
}

#[tokio::test]
async fn native_plugin_catalog_is_lazy_and_calls_keep_the_exact_result() {
    let (host, plugin, tool) = fixture();
    assert!(host.sender.lock().unwrap().is_none());
    assert_eq!(tool.descriptor["x-native-test"]["untouched"], true);
    let value = host
        .call(&tool, json!({"unicode":"原文", "nested":[1,true]}))
        .await
        .unwrap();
    assert_eq!(value["structuredContent"]["arguments"]["unicode"], "原文");
    assert_eq!(value["_meta"]["vendor"]["opaque"], true);
    assert_eq!(
        host.call(&tool, json!({"error":true})).await.unwrap_err(),
        "exact native business error"
    );
    assert_eq!(plugin.calls.load(Ordering::SeqCst), 2);
    let report = host.shutdown().await;
    assert_eq!(report.len(), 1);
    assert!(report[0].1.is_ok());
    assert_eq!(plugin.cleanups.load(Ordering::SeqCst), 1);
    assert_eq!(host.shutdown().await, report);
}

#[tokio::test]
async fn native_plugin_pending_tasks_do_not_serialize_calls_or_lose_audit_scope() {
    let registry = Registry::new();
    let plugin = Fixture::new();
    let second = Fixture::new();
    let host = Arc::new(
        NativeHost::new(
            registry.clone(),
            vec![
                Definition::new(plugin.clone()),
                Definition::new(Arc::new(Second(second.clone()))),
            ],
        )
        .unwrap(),
    );
    for (provider, tools) in host.catalogs().unwrap() {
        registry.replace(provider, tools).unwrap();
    }
    let tool = registry.lookup("ssmcp.fs.fixture").unwrap();
    let other_tool = registry.lookup("ssmcp.context.fixture").unwrap();
    let slow_host = host.clone();
    let slow_tool = tool.clone();
    // tokio::spawn also proves that the public host future remains Send even
    // though its actual ABI task and lease are deliberately !Send.
    let slow = tokio::spawn(async move { slow_host.call(&slow_tool, json!({"hold":true})).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), plugin.entered.notified())
        .await
        .unwrap();
    let observed = HarnessCall::new(41);
    let result = crate::observability::REQUEST_ID
        .scope(
            "source-request".into(),
            observed.scope(tokio::time::timeout(
                std::time::Duration::from_secs(5),
                host.call(&other_tool, json!({"fast":true})),
            )),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["structuredContent"]["request_id"], "source-request");
    assert_eq!(result["structuredContent"]["harness_call"], 41);
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        host.call(&tool, json!({"same_plugin":true})),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(!slow.is_finished());
    plugin.release.notify_one();
    slow.await.unwrap().unwrap();
    assert_eq!(plugin.threads.lock().unwrap().len(), 1);
    assert_eq!(
        *plugin.threads.lock().unwrap(),
        *second.threads.lock().unwrap()
    );
    assert!(
        host.shutdown()
            .await
            .into_iter()
            .all(|(_, result)| result.is_ok())
    );
}

#[tokio::test]
async fn native_plugin_registry_denials_happen_before_plugin_execution() {
    let (host, plugin, tool) = fixture();
    host.registry.invalidate(Provider::Workspace);
    assert!(host.call(&tool, json!({})).await.is_err());
    assert_eq!(plugin.calls.load(Ordering::SeqCst), 0);
    assert!(host.sender.lock().unwrap().is_none());
    assert!(
        host.shutdown()
            .await
            .into_iter()
            .all(|(_, result)| result.is_ok())
    );
}

#[tokio::test]
async fn native_plugin_cancellation_drops_the_real_future_inside_its_audit_scope() {
    let (host, plugin, tool) = fixture();
    let pending_host = host.clone();
    let pending_tool = tool.clone();
    let pending = tokio::spawn(async move {
        let observed = HarnessCall::new(73);
        crate::observability::REQUEST_ID
            .scope(
                "cancelled-request".into(),
                observed.scope(pending_host.call(&pending_tool, json!({"hold":true}))),
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), plugin.entered.notified())
        .await
        .unwrap();
    pending.abort();
    assert!(pending.await.unwrap_err().is_cancelled());
    tokio::time::timeout(std::time::Duration::from_secs(5), plugin.dropped.notified())
        .await
        .unwrap();
    assert_eq!(plugin.active.load(Ordering::SeqCst), 0);
    assert_eq!(plugin.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(
        *plugin.cancelled_scope.lock().unwrap(),
        Some((Some("cancelled-request".into()), Some(73)))
    );
    // A cancelled request must not leave a result slot or poison the instance.
    host.call(&tool, json!({"after_cancel":true}))
        .await
        .unwrap();
    assert!(
        host.shutdown()
            .await
            .into_iter()
            .all(|(_, result)| result.is_ok())
    );
}

#[tokio::test]
async fn native_plugin_shutdown_cancels_calls_before_cleanup_and_rejects_new_work() {
    let (host, plugin, tool) = fixture();
    let pending_host = host.clone();
    let pending_tool = tool.clone();
    let pending =
        tokio::spawn(async move { pending_host.call(&pending_tool, json!({"hold":true})).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), plugin.entered.notified())
        .await
        .unwrap();
    let report = tokio::time::timeout(std::time::Duration::from_secs(5), host.shutdown())
        .await
        .unwrap();
    assert!(
        report.iter().all(|(_, result)| result.is_ok()),
        "{report:?}"
    );
    assert!(pending.await.unwrap().is_err());
    assert_eq!(plugin.active.load(Ordering::SeqCst), 0);
    assert_eq!(plugin.cancelled.load(Ordering::SeqCst), 1);
    assert_eq!(plugin.cleanups.load(Ordering::SeqCst), 1);
    assert!(host.call(&tool, json!({})).await.is_err());
    assert_eq!(host.shutdown().await, report);
}

#[tokio::test]
async fn native_plugin_preserves_large_native_images_and_business_result_fields() {
    let (host, _plugin, tool) = fixture();
    // These sizes intentionally exceed both legacy dynamic-Extension limits.
    // Builtin tools keep their own existing budgets and the core output guard.
    let original = json!({
        "content":[{"type":"image","mimeType":"image/png","data":"a".repeat(300*1024)}],
        "structuredContent":{"text":"b".repeat(80*1024)},
        "isError":true,"_meta":{"opaque":{"list":[1,"原文",false]}}
    });
    let result = host
        .call(&tool, json!({"raw_result":original}))
        .await
        .unwrap();
    assert_eq!(result, original);
    assert!(
        host.shutdown()
            .await
            .into_iter()
            .all(|(_, result)| result.is_ok())
    );
}

#[tokio::test]
async fn native_plugin_queued_route_rechecks_the_registration_generation() {
    let registry = Registry::new();
    let plugin = Fixture::new();
    let definition = Definition::new(plugin.clone());
    for (provider, tools) in definition.catalog().unwrap() {
        registry.replace(provider, tools).unwrap();
    }
    let tool = registry.lookup("ssmcp.fs.fixture").unwrap();
    registry.validate(&tool).unwrap();
    let (sender, receiver) = mpsc::channel(1);
    let (reply, result) = oneshot::channel();
    let calls = Arc::new(Semaphore::new(1));
    sender
        .try_send(Request {
            number: 1,
            route: 0,
            call: Call {
                id: tool.id,
                provider: tool.provider,
                native_name: tool.native_name,
                generation: tool.generation,
                arguments: json!({}),
            },
            scope: RequestScope::capture(),
            reply,
            permit: calls.try_acquire_owned().unwrap(),
        })
        .unwrap_or_else(|_| panic!("private fixture queue unexpectedly full"));
    // Replace the still-existing route after admission but before the executor
    // sees it. A matching name alone must not authorize the queued old handle.
    let mut next = plugin.catalog();
    next[0].1[0]["description"] = json!("replacement generation");
    registry
        .replace(Provider::Workspace, next.pop().unwrap().1)
        .unwrap();
    let (stop, stopped) = watch::channel(false);
    let assertions = async {
        let error = result.await.unwrap().unwrap_err();
        assert!(error.contains("registration changed"), "{error}");
        assert_eq!(plugin.calls.load(Ordering::SeqCst), 0);
        stop.send_replace(true);
    };
    let local = LocalSet::new();
    let (report, ()) = local
        .run_until(async {
            tokio::join!(
                run(
                    Arc::new(vec![definition]),
                    registry,
                    Arc::new(AtomicU64::new(2)),
                    receiver,
                    stopped
                ),
                assertions
            )
        })
        .await;
    assert!(report.into_iter().all(|(_, result)| result.is_ok()));
}

#[tokio::test]
async fn native_plugin_abi_consumes_results_once_and_releases_unpolled_tasks() {
    let plugin = Fixture::new();
    let definition = Definition::new(plugin.clone());
    let mut instance = definition.instance().unwrap();
    let call = Call {
        id: "ssmcp.fs.fixture".into(),
        provider: Provider::Workspace,
        native_name: "ssmcp.fs.fixture".into(),
        generation: 1,
        arguments: json!({}),
    };
    let input = abi::input(&call).unwrap();
    let mut unpolled = instance.start(1, &call.native_name, &input).unwrap();
    assert!(instance.start(1, &call.native_name, &input).is_err());
    assert!(instance.finish(1).is_err());
    unpolled.cancel().unwrap();
    drop(unpolled);
    assert!(instance.finish(1).is_err());
    assert_eq!(plugin.calls.load(Ordering::SeqCst), 0);
    instance
        .start(2, &call.native_name, &input)
        .unwrap()
        .await
        .unwrap();
    let bytes = instance.finish(2).unwrap();
    assert!(abi::result(&bytes).is_ok());
    assert!(instance.finish(2).is_err());
    instance.shutdown(3).unwrap().await.unwrap();
    assert!(abi::result(&instance.finish(3).unwrap()).is_ok());
    assert!(instance.start(4, &call.native_name, &input).is_err());
    assert_eq!(plugin.cleanups.load(Ordering::SeqCst), 1);
}
