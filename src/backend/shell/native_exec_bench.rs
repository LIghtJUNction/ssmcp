//! Run on Linux with `cargo test --release --lib native_exec_throughput -- --ignored --nocapture`.
//! Measures the native start/wait/read path in waves. Fixture setup, cache priming,
//! journal verification and JSON reporting are outside the measured wave intervals.
use super::{Shell, artifact_tests::source};
use crate::runtime::call_scope::Source;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{sync::Barrier, task::JoinSet};

const CONCURRENCY: usize = 16;
const SCRIPT: &str = "/bin/sleep 0.025 && printf 'native-out' && printf 'native-err' >&2";

#[derive(Clone, Copy, Debug)]
enum Mode {
    Independent,
    InFlight,
    Warm,
}

fn arguments(mode: Mode) -> Value {
    let mut args = json!({"mode":"pipes","stdin":"null","command":["/bin/sh","-c",SCRIPT]});
    if !matches!(mode, Mode::Independent) {
        let ttl = if matches!(mode, Mode::Warm) {
            60_000
        } else {
            0
        };
        args["reuse"] = json!({"input_version":"constant-output-v1","ttl_ms":ttl});
    }
    args
}

struct Response {
    id: String,
    reuse: Option<String>,
    latency: Duration,
}

async fn execute(shell: &Shell, args: Value, owner: Source) -> Response {
    let begin = Instant::now();
    let start = shell
        .call_scoped("ssmcp.exec.start", args, owner.clone())
        .await
        .unwrap();
    let id = start["job_id"].as_str().unwrap().to_owned();
    let wait = shell
        .call_scoped(
            "ssmcp.exec.wait",
            json!({"job_id":id,"wait_ms":10000}),
            owner.clone(),
        )
        .await
        .unwrap();
    assert_eq!(wait["exit_code"], 0, "{wait}");
    assert_eq!(wait["running"], false);
    let read = shell
        .call_scoped("ssmcp.exec.read", json!({"job_id":id}), owner)
        .await
        .unwrap();
    let latency = begin.elapsed();
    assert_eq!(read["job_id"], id);
    assert_eq!(read["exit_code"], 0);
    assert_eq!(read["stdout"], "native-out");
    assert_eq!(read["stderr"], "native-err");
    for stream in ["stdout", "stderr"] {
        assert_eq!(read[format!("{stream}_eof")], true);
        assert_eq!(read[format!("{stream}_truncated")], false);
    }
    Response {
        id,
        reuse: start["reuse"]["status"].as_str().map(str::to_owned),
        latency,
    }
}

// Drain between waves so bounded journal eviction never undercounts real starts.
async fn started_jobs(shell: &Shell, owner: &Source, cursor: &mut u64) -> HashSet<String> {
    let mut ids = HashSet::new();
    loop {
        let page = shell
            .call_scoped(
                "ssmcp.exec.events",
                json!({"cursor":*cursor,"max_events":256}),
                owner.clone(),
            )
            .await
            .unwrap();
        assert_eq!(page["truncated"], false);
        for event in page["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| &item["event"])
        {
            if event["type"] == "exec.started" {
                assert!(ids.insert(event["job_id"].as_str().unwrap().to_owned()));
            }
        }
        *cursor = page["next_cursor"].as_u64().unwrap();
        if *cursor == page["end_cursor"].as_u64().unwrap() {
            return ids;
        }
    }
}

async fn measure(mode: Mode, count: usize) {
    let shell = Shell::new();
    let args = arguments(mode);
    let owner = source('a', "42", 81, 1);
    let mut cursor = 0;
    let warm_id = if matches!(mode, Mode::Warm) {
        let first = execute(&shell, args.clone(), owner.clone()).await;
        assert_eq!(first.reuse.as_deref(), Some("started"));
        assert_eq!(
            started_jobs(&shell, &owner, &mut cursor).await,
            HashSet::from([first.id.clone()])
        );
        Some(first.id)
    } else {
        None
    };

    let mut ids = HashSet::new();
    let mut physical = HashSet::new();
    let (mut joined, mut cached) = (0, 0);
    let mut latencies = Vec::with_capacity(count);
    let mut elapsed = Duration::ZERO;
    for base in (0..count).step_by(CONCURRENCY) {
        let wave = CONCURRENCY.min(count - base);
        let barrier = Arc::new(Barrier::new(wave + 1));
        let mut tasks = JoinSet::new();
        for index in base..base + wave {
            let shell = shell.clone();
            let args = args.clone();
            let barrier = barrier.clone();
            let owner = source('a', "42", 81, index as u64 + 2);
            tasks.spawn(async move {
                barrier.wait().await;
                execute(&shell, args, owner).await
            });
        }
        let begin = Instant::now();
        barrier.wait().await;
        let mut replies = Vec::with_capacity(wave);
        while let Some(reply) = tasks.join_next().await {
            replies.push(reply.unwrap());
        }
        elapsed += begin.elapsed();
        for response in replies {
            match (mode, response.reuse.as_deref()) {
                (Mode::Independent, None) | (Mode::InFlight, Some("started")) => (),
                (Mode::InFlight, Some("joined")) => joined += 1,
                (Mode::Warm, Some("cached")) => cached += 1,
                other => panic!("unexpected start receipt {other:?}"),
            }
            ids.insert(response.id);
            latencies.push(response.latency.as_micros());
        }
        for id in started_jobs(&shell, &owner, &mut cursor).await {
            assert!(physical.insert(id));
        }
    }
    match mode {
        Mode::Independent => {
            assert_eq!(ids.len(), count);
            assert_eq!(ids, physical);
        }
        Mode::InFlight => {
            assert_eq!(ids, physical);
            assert!(joined > 0);
            assert_eq!(joined + physical.len(), count);
            assert!(physical.len() >= count.div_ceil(CONCURRENCY));
        }
        Mode::Warm => {
            assert_eq!(ids, HashSet::from([warm_id.unwrap()]));
            assert_eq!(cached, count);
            assert!(physical.is_empty());
        }
    }
    assert_eq!(latencies.len(), count);
    latencies.sort_unstable();
    let percentile = |p: usize| latencies[(p * count).div_ceil(100) - 1];
    println!(
        "{}",
        json!({
            "benchmark":"ssmcp_native_exec","mode":format!("{mode:?}"),"concurrency":CONCURRENCY,
            "requests":count,"verified_successes":latencies.len(),"unique_job_ids":ids.len(),
            "measured_physical_starts":physical.len(),"priming_starts":usize::from(matches!(mode,Mode::Warm)),
            "joined":joined,"cached":cached,"active_elapsed_ms":elapsed.as_secs_f64()*1000.0,
            "requests_per_second":count as f64/elapsed.as_secs_f64(),"p50_us":percentile(50),"p95_us":percentile(95)
        })
    );
    shell.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "opt-in native shell throughput benchmark (real subprocesses)"]
async fn native_exec_throughput() {
    for count in [100, 1000] {
        for mode in [Mode::Independent, Mode::InFlight, Mode::Warm] {
            measure(mode, count).await;
        }
    }
}
