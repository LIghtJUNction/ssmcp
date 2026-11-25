use super::*;
use crate::{
    backend::shell::Shell,
    runtime::call_scope::CallScope,
    wire::{CodeContext, ControlAccount, ControlAgent, ControlAttribution},
};
use serde_json::json;
use std::{
    future::{Future, ready},
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
};
use tokio::time::Instant;

fn source(connection: u64, subject: &str, digit: char, request: u64) -> Source {
    let client = "reuse-test";
    let thread = format!("agent_{}", digit.to_string().repeat(32));
    let mut source = Source::default();
    source.call = Some(
        CallScope::from_relay_for_client(
            connection,
            Some(client),
            request,
            Some(&ControlAttribution {
                account: ControlAccount {
                    provider: "github".into(),
                    subject: subject.into(),
                    login: "test".into(),
                },
                owner: None,
                agent: Some(ControlAgent {
                    id: thread.clone(),
                    name: "worker".into(),
                }),
            }),
            Some(&CodeContext {
                run_id: format!("{request:032x}"),
                code: "test".into(),
                timestamp_ms: 1,
                session_id: Some(client.into()),
                thread_id: Some(thread),
            }),
        )
        .unwrap(),
    );
    source
}

#[test]
fn keys_follow_verified_scope_and_command_semantics() {
    let base = source(1, "123", 'a', 1);
    let argv = vec!["ab".into(), "c".into()];
    let env = HashMap::from([("A".into(), "a".into()), ("B".into(), "b".into())]);
    let build = |src: &Source,
                 dir: &str,
                 args: &[String],
                 vars: &HashMap<String, String>,
                 timeout,
                 rev: &str| {
        Key::new(
            src,
            Path::new(dir),
            args,
            Some(vars),
            timeout,
            Some("probe"),
            rev,
        )
    };
    let key = build(&base, "root", &argv, &env, 100, "v1").unwrap();
    let later = source(1, "123", 'a', 2);
    assert!(key == build(&later, "root", &argv, &env, 100, "v1").unwrap());
    for other in [
        source(2, "123", 'a', 1),
        source(1, "456", 'a', 1),
        source(1, "123", 'b', 1),
    ] {
        assert!(key != build(&other, "root", &argv, &env, 100, "v1").unwrap());
    }
    assert!(build(&Source::default(), "root", &argv, &env, 100, "v1").is_err());
    assert!(key != build(&base, "elsewhere", &argv, &env, 100, "v1").unwrap());
    assert!(key != build(&base, "root", &["a".into(), "bc".into()], &env, 100, "v1").unwrap());
    assert!(key != build(&base, "root", &argv, &HashMap::new(), 100, "v1").unwrap());
    assert!(key != build(&base, "root", &argv, &env, 101, "v1").unwrap());
    assert!(key != build(&base, "root", &argv, &env, 100, "v2").unwrap());
    assert!(key != Key::new(&base, Path::new("root"), &argv, Some(&env), 100, None, "v1").unwrap());
}

async fn completed(shell: &Shell) -> Arc<Job> {
    // Run the portable test harness with an empty filter match and no test bodies.
    let args = json!({"command":[std::env::current_exe().unwrap(), "--exact", "unused-reuse-fixture"],
                      "mode":"pipes"});
    let info = shell.call("ssmcp.exec.start", args).await.unwrap();
    let id = info["job_id"].as_str().unwrap();
    let done = shell
        .call("ssmcp.exec.wait", json!({"job_id":id,"wait_ms":5000}))
        .await
        .unwrap();
    assert_eq!(done["exit_code"], 0);
    shell.job(id).unwrap()
}

async fn assert_pending<F: Future>(future: Pin<&mut F>) {
    tokio::select! {
        biased;
        _ = future => panic!("a result was published before the leader finished startup"),
        () = ready(()) => {}
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_identical_requests_start_one_job_and_share_the_original_id() {
    let cache = Arc::new(Cache::default());
    let shell = Shell::new();
    let count = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(tokio::sync::Barrier::new(12));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..12 {
        let (cache, shell, count, barrier) =
            (cache.clone(), shell.clone(), count.clone(), barrier.clone());
        tasks.spawn(async move {
            barrier.wait().await;
            match cache
                .acquire(Key([0; 32]), MAX_TTL, |id| shell.job(id).ok())
                .await
                .unwrap()
            {
                Decision::Start(permit) => {
                    count.fetch_add(1, Ordering::Relaxed);
                    let job = completed(&shell).await;
                    permit.publish(&job.id);
                    job.id.clone()
                }
                Decision::Joined(job) | Decision::Cached(job) => job.id.clone(),
            }
        });
    }
    let expected = tasks.join_next().await.unwrap().unwrap();
    while let Some(result) = tasks.join_next().await {
        assert_eq!(result.unwrap(), expected);
    }
    assert_eq!(count.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn failed_start_wakes_waiters_and_does_not_replay_their_request() {
    let cache = Cache {
        capacity: 1,
        ..Cache::default()
    };
    let key = Key([1; 32]);
    let Decision::Start(leader) = cache.acquire(key, MAX_TTL, |_| None).await.unwrap() else {
        panic!()
    };
    let mut follower = Box::pin(cache.acquire(key, MAX_TTL, |_| None));
    assert_pending(follower.as_mut()).await;
    assert!(
        cache
            .acquire(Key([2; 32]), MAX_TTL, |_| None)
            .await
            .is_err()
    );
    drop(leader);
    assert!(follower.await.is_err());
    assert!(matches!(
        cache.acquire(key, MAX_TTL, |_| None).await,
        Ok(Decision::Start(_))
    ));
}

#[tokio::test]
async fn ttl_capacity_and_registry_eviction_control_hits() {
    let shell = Shell::new();
    let job = completed(&shell).await;
    let lookup = |id: &str| shell.job(id).ok();
    let cache = Cache {
        capacity: 1,
        ..Cache::default()
    };
    let key = Key([1; 32]);
    let Decision::Start(leader) = cache.acquire(key, MAX_TTL, lookup).await.unwrap() else {
        panic!()
    };
    leader.publish(&job.id);
    assert!(matches!(
        cache.acquire(key, MAX_TTL, lookup).await,
        Ok(Decision::Cached(_))
    ));
    job.state().running = true;
    assert!(matches!(
        cache.acquire(key, Duration::ZERO, lookup).await,
        Ok(Decision::Joined(_))
    ));
    assert!(cache.acquire(Key([2; 32]), MAX_TTL, lookup).await.is_err());
    job.state().running = false;
    let shorter = Duration::from_secs(1);
    job.state().finished = Some(Instant::now() - Duration::from_secs(2));
    assert!(matches!(
        cache.acquire(key, shorter, lookup).await,
        Ok(Decision::Start(_))
    ));
    let Decision::Start(leader) = cache.acquire(key, shorter, lookup).await.unwrap() else {
        panic!()
    };
    leader.publish(&job.id);
    assert!(matches!(
        cache.acquire(key, MAX_TTL, lookup).await,
        Ok(Decision::Start(_))
    ));
    job.state().finished = Some(Instant::now());
    let Decision::Start(leader) = cache.acquire(key, Duration::ZERO, lookup).await.unwrap() else {
        panic!()
    };
    leader.publish(&job.id);
    assert!(matches!(
        cache.acquire(key, MAX_TTL, lookup).await,
        Ok(Decision::Start(_))
    ));
    let Decision::Start(leader) = cache.acquire(key, MAX_TTL, lookup).await.unwrap() else {
        panic!()
    };
    leader.publish(&job.id);
    assert!(matches!(
        cache.acquire(Key([2; 32]), MAX_TTL, lookup).await,
        Ok(Decision::Start(_))
    ));
    assert!(cache.acquire(key, MAX_TTL + shorter, lookup).await.is_err());
    let Decision::Start(leader) = cache.acquire(key, MAX_TTL, lookup).await.unwrap() else {
        panic!()
    };
    leader.publish(&job.id);
    assert!(shell.registry().forget(&job.id));
    assert!(matches!(
        cache.acquire(key, MAX_TTL, lookup).await,
        Ok(Decision::Start(_))
    ));
}

#[tokio::test]
async fn only_success_with_both_complete_streams_is_cacheable() {
    let shell = Shell::new();
    let job = completed(&shell).await;
    assert!(matches!(classify(&job, MAX_TTL), Some(Hit::Success)));
    for (code, signal, error, cancelled, timed_out) in [
        (Some(2), None, None, false, false),
        (None, None, None, false, false),
        (Some(0), Some(15), None, false, false),
        (Some(0), None, Some("failed"), false, false),
        (Some(0), None, None, true, false),
        (Some(0), None, None, false, true),
    ] {
        {
            let mut state = job.state();
            state.exit_code = code;
            state.exit_signal = signal;
            state.error = error.map(str::to_owned);
            state.cancelled = cancelled;
            state.timed_out = timed_out;
        }
        assert!(classify(&job, MAX_TTL).is_none());
    }
    job.state().timed_out = false;
    job.state().stderr.eof = false;
    assert!(classify(&job, MAX_TTL).is_none());
    job.state().stderr.eof = true;
    job.state().stdout.start = 1;
    assert!(classify(&job, MAX_TTL).is_none());
    job.state().stdout.start = 0;
    job.state().stdout.end += 1;
    assert!(classify(&job, MAX_TTL).is_none());
}
