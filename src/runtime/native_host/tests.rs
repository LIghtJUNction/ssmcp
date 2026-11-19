use super::*;
use serde_json::json;
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fixture {
    calls: AtomicUsize,
    cleanups: AtomicUsize,
    entered: Notify,
    release: Notify,
    threads: Mutex<HashSet<std::thread::ThreadId>>,
}

impl Fixture {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: AtomicUsize::new(0),
            cleanups: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Notify::new(),
            threads: Mutex::new(HashSet::new()),
        })
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
            self.threads
                .lock()
                .unwrap()
                .insert(std::thread::current().id());
            if call.arguments["hold"] == true {
                self.entered.notify_one();
                self.release.notified().await;
            }
            if call.arguments["error"] == true {
                return Err("exact native business error".into());
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
            self.cleanups.fetch_add(1, Ordering::SeqCst);
            Ok(Value::Null)
        })
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
    let (host, plugin, tool) = fixture();
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
                host.call(&tool, json!({"fast":true})),
            )),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result["structuredContent"]["request_id"], "source-request");
    assert_eq!(result["structuredContent"]["harness_call"], 41);
    assert!(!slow.is_finished());
    plugin.release.notify_one();
    slow.await.unwrap().unwrap();
    assert_eq!(plugin.threads.lock().unwrap().len(), 1);
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
