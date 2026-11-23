//! Explicit, per-provider initialization. Observation never starts a process.
use crate::registry::{OPTIONAL_PROVIDERS as PROVIDERS, Provider};
use futures_util::{
    FutureExt,
    future::{BoxFuture, Shared, join_all},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{collections::BTreeMap, future::Future, sync::Mutex};
use tokio::task::AbortHandle;

pub(super) fn tools() -> Vec<Value> {
    vec![
        json!({
            "name":"ssmcp.harnesses.list",
            "description":"Read cached status for optional external harnesses. The client starts with native tools only. not_loaded means the harness has not been probed or started; installed versions are unknown until an explicit load. This observation does not start or probe any external program.",
            "inputSchema":{"type":"object","properties":{},"additionalProperties":false},
            "annotations":{"readOnlyHint":true,"destructiveHint":false}
        }),
        json!({
            "name":"ssmcp.harnesses.load",
            "description":"Explicitly load one optional external harness when its capabilities are needed. Native ssmcp tools work without any harness. This may probe the selected installed executable and start its native protocol bridge, but does not start a model turn. Only a successful native handshake and catalog enumeration register tools. Concurrent loads share one initialization. Success and failure are cached; refresh:true explicitly retries or refreshes only this provider. Inspect the current tool catalog after loading. No packages are installed and no other provider is scanned.",
            "inputSchema":{"type":"object","required":["provider"],"additionalProperties":false,"properties":{
                "provider":{"type":"string","enum":PROVIDERS.iter().map(|(_, name)| *name).collect::<Vec<_>>()},
                "refresh":{"type":"boolean","default":false}
            }},
            "annotations":{"readOnlyHint":false,"destructiveHint":false}
        }),
    ]
}

pub(super) fn initial_diagnostics() -> BTreeMap<Provider, Value> {
    PROVIDERS
        .iter()
        .map(|(provider, name)| {
            (
                *provider,
                json!({
                    "provider":name,"status":"not_loaded","tools":0,
                    "executable":null,"version":null,"activation":"explicit",
                    "reason":"Optional harness has not been probed or started. Use native tools first; explicitly load this provider only when needed."
                }),
            )
        })
        .collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    provider: String,
    #[serde(default)]
    refresh: bool,
}

pub(super) fn parse(arguments: Value) -> Result<(Provider, bool), String> {
    let request: Request = serde_json::from_value(arguments).map_err(|_| {
        "harness load requires provider and accepts only boolean refresh".to_owned()
    })?;
    let provider = PROVIDERS
        .iter()
        .find(|(_, name)| *name == request.provider)
        .map(|(provider, _)| *provider)
        .ok_or("third-party harness import was removed; use external tools or explicit terminal/exec. Only a configured ssmcp peer can be attached here")?;
    Ok((provider, request.refresh))
}

pub(super) type Pending = Shared<BoxFuture<'static, Result<Value, String>>>;

struct Attempt {
    pending: Pending,
    abort: AbortHandle,
}

#[derive(Default)]
struct State {
    closed: bool,
    attempts: BTreeMap<Provider, Attempt>,
}

#[derive(Default)]
pub(super) struct Loads {
    state: Mutex<State>,
}

impl Loads {
    /// The stored task outlives any individual waiter. Nothing is spawned until
    /// an explicit request enters this method; failed results remain cached.
    pub(super) fn request<F>(
        &self,
        provider: Provider,
        refresh: bool,
        start: F,
    ) -> Result<Pending, String>
    where
        F: Future<Output = Result<Value, String>> + Send + 'static,
    {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.closed {
            return Err("client connection is closed".into());
        }
        if let Some(attempt) = state.attempts.get(&provider)
            && (!refresh || !attempt.abort.is_finished())
        {
            return Ok(attempt.pending.clone());
        }
        let task = tokio::spawn(start);
        let abort = task.abort_handle();
        let pending = async move {
            task.await
                .map_err(|_| "optional harness loading task stopped".to_owned())?
        }
        .boxed()
        .shared();
        state.attempts.insert(
            provider,
            Attempt {
                pending: pending.clone(),
                abort,
            },
        );
        Ok(pending)
    }

    /// Wait only for explicitly requested work. An untouched backend is ready
    /// immediately; this method never discovers or enables a provider.
    pub(super) async fn wait(&self) {
        let pending = {
            let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state
                .attempts
                .values()
                .map(|attempt| attempt.pending.clone())
                .collect::<Vec<_>>()
        };
        let _ = join_all(pending).await;
    }

    fn close(&self) -> Vec<Attempt> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.closed = true;
        std::mem::take(&mut state.attempts).into_values().collect()
    }

    pub(super) async fn shutdown(&self) {
        let attempts = self.close();
        for attempt in &attempts {
            attempt.abort.abort();
        }
        let _ = join_all(attempts.into_iter().map(|attempt| attempt.pending)).await;
    }

    pub(super) fn abort(&self) {
        for attempt in self.close() {
            attempt.abort.abort();
        }
    }
}

impl Drop for Loads {
    fn drop(&mut self) {
        self.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };
    use tokio::sync::{Notify, Semaphore};

    #[test]
    fn untouched_providers_have_no_invented_installation_metadata() {
        let diagnostics = initial_diagnostics();
        assert_eq!(diagnostics.len(), PROVIDERS.len());
        for value in diagnostics.values() {
            assert_eq!(value["status"], "not_loaded");
            assert_eq!(value["tools"], 0);
            assert!(value["executable"].is_null());
            assert!(value["version"].is_null());
        }
        assert_eq!(tools()[0]["annotations"]["readOnlyHint"], true);
        assert_eq!(tools()[1]["annotations"]["readOnlyHint"], false);
    }

    #[test]
    fn load_request_rejects_unknown_providers_and_extra_arguments() {
        assert_eq!(
            parse(json!({"provider":"ssmcp"})).unwrap(),
            (Provider::Peer, false)
        );
        assert_eq!(
            parse(json!({"provider":"ssmcp","refresh":true})).unwrap(),
            (Provider::Peer, true)
        );
        for arguments in [
            json!({}),
            json!({"provider":"native"}),
            json!({"provider":"codex"}),
            json!({"provider":"claude"}),
            json!({"provider":"pi"}),
            json!({"provider":"opencode"}),
            json!({"provider":"playwright"}),
            json!({"provider":"codex","refresh":"true"}),
            json!({"provider":"codex","command":["other-program"]}),
            json!({"provider":"codex","token":"not-accepted"}),
        ] {
            assert!(parse(arguments).is_err());
        }
    }

    #[tokio::test]
    async fn observation_wait_does_not_start_any_optional_provider() {
        let loads = Loads::default();
        loads.wait().await;
        assert!(loads.state.lock().unwrap().attempts.is_empty());
        loads.shutdown().await;
    }

    #[tokio::test]
    async fn concurrent_requests_and_refresh_share_one_running_initialization() {
        let loads = Loads::default();
        let count = Arc::new(AtomicUsize::new(0));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let first = loads
            .request(Provider::Codex, false, {
                let count = count.clone();
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    entered.notify_one();
                    let _permit = release.acquire().await.unwrap();
                    Ok(json!({"peer":17}))
                }
            })
            .unwrap();
        entered.notified().await;
        let second = loads
            .request(Provider::Codex, false, async {
                Err("duplicate initialization".into())
            })
            .unwrap();
        let refreshing = loads
            .request(Provider::Codex, true, async {
                Err("refresh duplicated an in-flight initialization".into())
            })
            .unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
        release.add_permits(1);
        let (a, b, c) = tokio::join!(first, second, refreshing);
        assert_eq!(a.unwrap(), json!({"peer":17}));
        assert_eq!(b.unwrap(), json!({"peer":17}));
        assert_eq!(c.unwrap(), json!({"peer":17}));
        assert_eq!(
            loads
                .request(Provider::Codex, false, async {
                    Err("cached success was restarted".into())
                })
                .unwrap()
                .await
                .unwrap(),
            json!({"peer":17})
        );
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_waiter_does_not_cancel_the_shared_initialization() {
        let loads = Loads::default();
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Semaphore::new(0));
        let pending = loads
            .request(Provider::Claude, false, {
                let entered = entered.clone();
                let release = release.clone();
                async move {
                    entered.notify_one();
                    let _permit = release.acquire().await.unwrap();
                    Ok(json!({"peer":23}))
                }
            })
            .unwrap();
        entered.notified().await;
        let waiter = tokio::spawn(pending.clone());
        waiter.abort();
        assert!(waiter.await.unwrap_err().is_cancelled());
        release.add_permits(1);
        assert_eq!(pending.await.unwrap(), json!({"peer":23}));
    }

    #[tokio::test]
    async fn failed_attempt_is_cached_until_an_explicit_refresh() {
        let loads = Loads::default();
        let count = Arc::new(AtomicUsize::new(0));
        let first = loads
            .request(Provider::Pi, false, {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err("fixture unavailable".into())
                }
            })
            .unwrap();
        assert_eq!(first.await.unwrap_err(), "fixture unavailable");
        let cached = loads
            .request(Provider::Pi, false, {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"unexpected":"retry"}))
                }
            })
            .unwrap();
        assert_eq!(cached.await.unwrap_err(), "fixture unavailable");
        assert_eq!(count.load(Ordering::SeqCst), 1);
        let retried = loads
            .request(Provider::Pi, true, {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Ok(json!({"ready":true}))
                }
            })
            .unwrap();
        assert_eq!(retried.await.unwrap(), json!({"ready":true}));
        assert_eq!(count.load(Ordering::SeqCst), 2);
        assert_eq!(
            loads
                .request(Provider::Claude, false, async {
                    Ok(json!({"independent":true}))
                })
                .unwrap()
                .await
                .unwrap(),
            json!({"independent":true})
        );
    }

    #[tokio::test]
    async fn shutdown_cancels_loading_and_rejects_new_provider_work() {
        struct Dropped(Arc<AtomicBool>);
        impl Drop for Dropped {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let loads = Loads::default();
        let entered = Arc::new(Notify::new());
        let dropped = Arc::new(AtomicBool::new(false));
        let pending = loads
            .request(Provider::Claude, false, {
                let entered = entered.clone();
                let dropped = dropped.clone();
                async move {
                    let _guard = Dropped(dropped);
                    entered.notify_one();
                    std::future::pending::<()>().await;
                    Ok(Value::Null)
                }
            })
            .unwrap();
        entered.notified().await;
        loads.shutdown().await;
        assert!(pending.await.is_err());
        assert!(dropped.load(Ordering::SeqCst));
        assert!(
            loads
                .request(Provider::Codex, false, async {
                    panic!("closed loader must never poll this work")
                })
                .is_err()
        );
    }
}
