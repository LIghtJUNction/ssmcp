//! Registration-only tests with real, isolated transport subprocesses.
//! No backend, installed coding harness, network, credentials, or model is used.
use super::*;
use serde_json::Value;
use std::{
    cell::Cell,
    fs,
    io::{Read, Write},
    path::Path,
    process::Stdio,
};

const PEER: &str = r#"
import os, sys
fixture = os.environ["SSMCP_PROTOCOL_FIXTURE"]
scenario = os.environ["SSMCP_PROTOCOL_SCENARIO"]

def trace(stage):
    try:
        with open(os.path.join(fixture, "startup.log"), "a", encoding="utf-8") as log:
            log.write(f"{scenario}: {stage}\n")
    except OSError:
        pass

trace(f"peer started ({sys.executable!r})")
import json, pathlib, time
root = pathlib.Path(fixture)
# A reconnect cannot register while the previous rejected subprocess is alive.
lease = open(root / "ssh-owner.lock", "a+b")
if os.name == "nt":
    import msvcrt
    if os.fstat(lease.fileno()).st_size == 0:
        lease.write(b"0"); lease.flush()
    lease.seek(0)
    msvcrt.locking(lease.fileno(), msvcrt.LK_NBLCK, 1)
else:
    import fcntl
    fcntl.flock(lease.fileno(), fcntl.LOCK_EX | fcntl.LOCK_NB)
trace("lease acquired")

if scenario.startswith("guarded-repo-"):
    resolution = json.loads(sys.stdin.readline())
    assert resolution == {"type":"resolve_controller","id":0,"principal":"octocat@ssmcp.test"}
    print(json.dumps({"type":"resolved_controller","id":0,"principal":resolution["principal"],
                      "provider":"github","subject":"42"}), flush=True)

hello = json.loads(sys.stdin.readline())
assert hello["type"] == "hello"
trace("Hello received")
with open(root / "hellos.jsonl", "a", encoding="utf-8") as recorded:
    recorded.write(json.dumps(hello) + "\n")

def respond(frame):
    trace("response about to be sent")
    print(frame, flush=True)

def reject(message):
    respond(json.dumps({"type":"reject", "message":message}))
    # The client must terminate and reap this process before a later reconnect.
    time.sleep(60)
    raise SystemExit(2)

unsupported = "unsupported ssmcp wire protocol"
if scenario == "unsupported":
    reject(unsupported)
if scenario == "unsupported-detail":
    reject(unsupported + "; server requires version 8")
if scenario == "auth":
    reject("GitHub access policy denied")
if scenario == "legacy-account-allow":
    reject("allow entries must use username@github; only GitHub is supported")
if scenario == "near-match":
    reject(unsupported + " ")
if scenario == "case-variant":
    reject("Unsupported ssmcp wire protocol")
if scenario == "stderr-only":
    trace("stderr about to be sent")
    print(unsupported, file=sys.stderr, flush=True)
    raise SystemExit(0)
if scenario == "eof":
    raise SystemExit(0)
if scenario == "malformed":
    respond('{"type":')
    raise SystemExit(0)
if scenario == "wrong-frame":
    respond(json.dumps({"type":"reply", "id":1, "result":{"Err":unsupported}}))
    raise SystemExit(0)
if scenario == "timeout":
    time.sleep(60)
    raise SystemExit(2)

github = bool(hello["metadata"].get("allow"))
code, ttl = ("", 0) if github else ("a" * 64, 600)
capabilities = []
if scenario == "session-access":
    capabilities = [os.environ["SSMCP_PROTOCOL_ACCESS_CAPABILITY"]]
elif scenario.startswith("guarded-repo-"):
    access = os.environ["SSMCP_PROTOCOL_ACCESS_CAPABILITY"]
    repo = "repo_events_v1"
    assert hello["metadata"]["allow"] == ["octocat@ssmcp.test"]
    parent_offer = os.environ["SSMCP_RELAY_CAPABILITIES"].split(",")
    assert access in parent_offer and "control_attribution_v1" in parent_offer
    assert (repo in parent_offer) == (scenario != "guarded-repo-unsolicited")
    capabilities = [access, "control_attribution_v1"]
    tracking = "git_tracking_v1"
    assert (tracking in parent_offer) == (repo in parent_offer)
    if scenario == "guarded-repo-tracking-only" or scenario == "guarded-repo-unsolicited":
        capabilities.append(tracking)
    if scenario not in ("guarded-repo-no-echo", "guarded-repo-tracking-only"):
        capabilities.append(repo)
elif scenario == "messages":
    capabilities = [os.environ["SSMCP_PROTOCOL_MESSAGES_CAPABILITY"]]
elif scenario == "session-messages":
    capabilities = [os.environ["SSMCP_PROTOCOL_ACCESS_CAPABILITY"],
                    "control_attribution_v1",
                    os.environ["SSMCP_PROTOCOL_MESSAGES_CAPABILITY"]]
if scenario == "policy-login":
    code, ttl = "a" * 64, 0
elif scenario == "policy-ttl":
    code, ttl = "", 600
elif scenario == "no-login":
    code, ttl = "", 600
elif scenario == "no-expiry":
    code, ttl = "a" * 64, 0
respond(json.dumps({"type":"ready", "url":"https://fixture.invalid/mcp",
                    "login_code":code, "login_expires_in":ttl, "client_id":"b"*64,
                    **({"capabilities":capabilities} if capabilities else {})}))
if scenario == "guarded-repo-negotiated":
    event = json.loads(sys.stdin.readline())
    assert event == {"type":"repository_event","event":{"kind":"pull_request_opened","remote":"origin","number":7}}
    respond(json.dumps({"type":"reply","id":7,"result":{"Ok":True}}))
# Keep registration live until its test explicitly performs normal cleanup.
sys.stdin.read()
"#;

struct Fixture {
    directory: PathBuf,
    python: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let mut random = [0u8; 16];
        crate::platform::random_bytes(&mut random).unwrap();
        let suffix: String = crate::hex::encode(random);
        let directory = std::env::temp_dir().join(format!("ssmcp-protocol-{suffix}"));
        fs::create_dir(&directory).unwrap();
        // A fixed Unix system interpreter avoids unrelated PATH-mutating tests.
        // Windows CI may expose either python3.exe or python.exe.
        let python = if cfg!(unix) && Path::new("/usr/bin/python3").is_file() {
            PathBuf::from("/usr/bin/python3")
        } else {
            ["python3", "python"]
                .into_iter()
                .find_map(|name| crate::platform::executable(name).ok())
                .expect("protocol fixture requires an available Python 3 interpreter")
        };
        Self { directory, python }
    }

    fn command(&self, scenario: &str) -> Command {
        if let Ok(mut log) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.join("startup.log"))
        {
            let _ = writeln!(log, "{scenario}: selected Python {:?}", self.python);
        }
        let mut command = Command::new(&self.python);
        command
            .args(["-u", "-c", PEER])
            .env("SSMCP_PROTOCOL_FIXTURE", &self.directory)
            .env("SSMCP_PROTOCOL_SCENARIO", scenario)
            .env(
                "SSMCP_PROTOCOL_ACCESS_CAPABILITY",
                crate::wire::SESSION_ACCESS_CAPABILITY,
            )
            .env(
                "SSMCP_PROTOCOL_MESSAGES_CAPABILITY",
                crate::wire::messages::CAPABILITY,
            )
            .env_remove("PYTHONHOME")
            .env_remove("PYTHONPATH")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        command
    }

    async fn register(
        &self,
        scenario: &str,
        metadata: &ClientMetadata,
        registered: &mut bool,
        deadline: Duration,
    ) -> (crate::Result<Option<RegisteredTransport>>, usize) {
        let count = Cell::new(0usize);
        let result = register_transport(
            TransportKind::Relay,
            None,
            metadata,
            registered,
            || {
                count.set(count.get() + 1);
                self.command(scenario)
            },
            std::future::pending(),
            deadline,
        )
        .await;
        self.report_registration(scenario, &result);
        (result, count.get())
    }

    fn report_registration(
        &self,
        scenario: &str,
        result: &crate::Result<Option<RegisteredTransport>>,
    ) {
        let outcome = result.as_ref().map(|registration| {
            if registration.is_some() {
                "Ready"
            } else {
                "cancelled"
            }
        });
        let mut trace = Vec::new();
        let trace_read = fs::File::open(self.directory.join("startup.log"))
            .and_then(|file| file.take(4096).read_to_end(&mut trace));
        // The test harness includes this output on failure, even if a later assertion
        // panics before inspecting the registration result.
        eprintln!(
            "protocol fixture {:?}, Python {:?}, scenario {scenario}: registration {outcome:?}\nstartup trace (up to 4096 bytes, read {trace_read:?}):\n{}",
            self.directory,
            self.python,
            String::from_utf8_lossy(&trace),
        );
    }

    fn hellos(&self) -> Vec<Value> {
        fs::read_to_string(self.directory.join("hellos.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.directory);
    }
}

fn metadata(allow: bool) -> ClientMetadata {
    ClientMetadata {
        name: "protocol-fixture".into(),
        title: Some("Retained local task title".into()),
        cwd: std::env::current_dir().unwrap().to_str().unwrap().into(),
        hostname: "fixture".into(),
        allow: if allow {
            vec!["octocat@ssmcp.test".into()]
        } else {
            Vec::new()
        },
    }
}

fn registration_error(result: crate::Result<Option<RegisteredTransport>>) -> crate::Error {
    match result {
        Err(error) => error,
        Ok(_) => panic!("registration unexpectedly succeeded"),
    }
}

async fn close(mut registration: RegisteredTransport) {
    stop_registration(
        &mut registration.process,
        &mut registration.stderr_pump,
        registration.transport_kind,
    )
    .await
    .unwrap();
    assert!(
        registration.process.id().is_none(),
        "transport child was not reaped"
    );
}

const READY_LIMIT: Duration = Duration::from_secs(10);

#[tokio::test]
async fn initial_allow_requires_capability_and_reaps_an_unacknowledged_transport() {
    let fixture = Fixture::new();
    let metadata = metadata(true);
    let mut registered = false;
    let (result, _) = fixture
        .register("current", &metadata, &mut registered, READY_LIMIT)
        .await;
    let mut registration = result.unwrap().unwrap();
    let error = registration
        .require_allow_access(&metadata.allow)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("account access controls"));
    assert!(error.downcast_ref::<relay::PermanentFailure>().is_some());
    assert!(
        registration.process.id().is_none(),
        "the unsupported relay was not reaped"
    );

    let (result, _) = fixture
        .register("session-access", &metadata, &mut registered, READY_LIMIT)
        .await;
    let mut registration = result.unwrap().unwrap();
    assert!(registration.access_supported);
    assert!(!registration.messaging_supported);
    registration
        .require_allow_access(&["colleague@ssmcp.test".into(), "alice@ssmcp.test".into()])
        .await
        .unwrap();
    close(registration).await;
}

#[tokio::test]
async fn current_protocol_preserves_metadata_for_login_and_allow() {
    for allow in [false, true] {
        let fixture = Fixture::new();
        let metadata = metadata(allow);
        let mut registered = false;
        let (result, attempts) = fixture
            .register("current", &metadata, &mut registered, READY_LIMIT)
            .await;
        let registration = result.unwrap().expect("registration was cancelled");
        assert_eq!(attempts, 1);
        assert!(registered);
        assert!(!registration.messaging_supported);
        assert_eq!(registration.ready.1.is_empty(), allow);
        assert_eq!(registration.ready.2 == 0, allow);
        assert_eq!(fixture.hellos()[0]["version"], VERSION);
        assert_eq!(
            fixture.hellos()[0]["metadata"],
            serde_json::to_value(&metadata).unwrap()
        );
        close(registration).await;
    }
}

#[tokio::test]
async fn current_protocol_negotiates_messaging_independently_of_access() {
    for (scenario, access_supported) in [("messages", false), ("session-messages", true)] {
        let fixture = Fixture::new();
        let mut registered = false;
        let (result, attempts) = fixture
            .register(scenario, &metadata(false), &mut registered, READY_LIMIT)
            .await;
        let registration = result.unwrap().expect("registration was cancelled");
        assert_eq!(attempts, 1);
        assert!(registered);
        assert_eq!(registration.access_supported, access_supported);
        assert!(registration.messaging_supported);
        close(registration).await;
    }
}

#[tokio::test]
async fn protocol_rejection_is_terminal_without_downgrade_and_reaps_transport() {
    for scenario in ["unsupported", "unsupported-detail"] {
        let fixture = Fixture::new();
        let metadata = metadata(true);
        let mut registered = false;
        let (result, attempts) = fixture
            .register(scenario, &metadata, &mut registered, READY_LIMIT)
            .await;
        let error = registration_error(result);
        assert!(error.downcast_ref::<UnsupportedWireProtocol>().is_some());
        assert_eq!(attempts, 1);
        assert!(!registered);
        assert_eq!(fixture.hellos().len(), 1);
        assert_eq!(fixture.hellos()[0]["version"], VERSION);
        assert_eq!(
            fixture.hellos()[0]["metadata"],
            serde_json::to_value(&metadata).unwrap()
        );

        // A later explicit connection still uses v7, and the prior process lock is gone.
        let (result, attempts) = fixture
            .register("current", &metadata, &mut registered, READY_LIMIT)
            .await;
        let registration = result.unwrap().expect("registration was cancelled");
        assert_eq!(attempts, 1);
        assert!(registered);
        assert_eq!(
            fixture
                .hellos()
                .iter()
                .map(|value| value["version"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            [u64::from(VERSION), u64::from(VERSION)]
        );
        close(registration).await;
    }
}

#[tokio::test]
async fn authentication_errors_and_untyped_failures_never_retry_registration() {
    for scenario in [
        "auth",
        "near-match",
        "case-variant",
        "stderr-only",
        "eof",
        "malformed",
        "wrong-frame",
    ] {
        let fixture = Fixture::new();
        let mut registered = false;
        let (result, attempts) = fixture
            .register(scenario, &metadata(true), &mut registered, READY_LIMIT)
            .await;
        let error = registration_error(result);
        assert_eq!(attempts, 1, "{scenario}: {error}");
        assert!(!registered, "{scenario}");
        assert!(
            error.downcast_ref::<UnsupportedWireProtocol>().is_none(),
            "{scenario}"
        );
        if scenario == "auth" {
            assert_eq!(error.to_string(), "GitHub access policy denied");
        }
    }
}

#[tokio::test]
async fn current_protocol_preserves_allow_checks_and_login_requirements() {
    for (scenario, allow, expected) in [
        (
            "policy-login",
            true,
            "did not acknowledge the explicit account allow policy",
        ),
        (
            "policy-ttl",
            true,
            "did not acknowledge the explicit account allow policy",
        ),
        ("no-login", false, "returned no account access policy"),
        ("no-expiry", false, "returned no account access policy"),
    ] {
        let fixture = Fixture::new();
        let metadata = metadata(allow);
        let original = metadata.clone();
        let mut registered = false;
        let (result, attempts) = fixture
            .register(scenario, &metadata, &mut registered, READY_LIMIT)
            .await;
        let error = registration_error(result);
        assert!(error.to_string().contains(expected), "{scenario}: {error}");
        assert_eq!(attempts, 1);
        assert!(
            registered,
            "a received Ready still suppresses unrelated onboarding"
        );
        assert_eq!(metadata, original);
        assert_eq!(fixture.hellos()[0]["version"], VERSION);
        assert_eq!(
            fixture.hellos()[0]["metadata"],
            serde_json::to_value(&original).unwrap()
        );
    }
}

#[tokio::test]
async fn handshake_deadline_stops_the_single_current_protocol_attempt() {
    let fixture = Fixture::new();
    let mut registered = false;
    // Only this timeout case uses a short deadline; successful subprocess tests
    // retain a separate generous startup allowance.
    let (result, attempts) = fixture
        .register(
            "timeout",
            &metadata(false),
            &mut registered,
            Duration::from_millis(200),
        )
        .await;
    let error = registration_error(result);
    assert_eq!(error.to_string(), "Relay transport registration timed out");
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::TimedOut
    );
    assert_eq!(attempts, 1);
    assert!(!registered);
}

#[tokio::test]
async fn cancelled_registration_does_not_start_a_transport() {
    let fixture = Fixture::new();
    let metadata = metadata(false);
    let mut registered = false;
    let attempts = Cell::new(0usize);
    let result = register_transport(
        TransportKind::Relay,
        None,
        &metadata,
        &mut registered,
        || {
            attempts.set(attempts.get() + 1);
            fixture.command("current")
        },
        std::future::ready(()),
        READY_LIMIT,
    )
    .await;
    fixture.report_registration("current", &result);
    let result = result.unwrap();
    assert!(result.is_none());
    assert_eq!(attempts.get(), 0);
    assert!(!registered);
}

#[tokio::test]
async fn guarded_repository_events_require_both_offer_and_echo() {
    const CHILD: &str = "SSMCP_GUARDED_REPOSITORY_TEST_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Admission records a real local controller. Give this process its own
        // ledger without changing the environment of unrelated parallel tests.
        let fixture = Fixture::new();
        let root = fixture.directory.join("auth");
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "client::protocol_tests::guarded_repository_events_require_both_offer_and_echo",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("SSMCP_AUTH_HOME", &root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(root.join("controller-accounts.json").is_file());
        return;
    }

    for (scenario, offered, enabled, git_tracking) in [
        ("guarded-repo-negotiated", true, true, false),
        ("guarded-repo-tracking-only", true, false, true),
        ("guarded-repo-no-echo", true, false, false),
        ("guarded-repo-unsolicited", false, false, false),
    ] {
        let fixture = Fixture::new();
        let metadata = metadata(true);
        let mut registered = false;
        let result = register_transport(
            TransportKind::Relay,
            Some("https://ssmcp.test"),
            &metadata,
            &mut registered,
            || {
                let mut command = fixture.command(scenario);
                let relay_command = relay::command(&fixture.python, "https://ssmcp.test");
                let offer = relay_command
                    .as_std()
                    .get_envs()
                    .find(|(name, _)| *name == "SSMCP_RELAY_CAPABILITIES")
                    .and_then(|(_, value)| value)
                    .unwrap();
                command.env("SSMCP_RELAY_CAPABILITIES", offer);
                if offered {
                    relay::offer_repository_events(&mut command);
                }
                command
            },
            std::future::pending(),
            READY_LIMIT,
        )
        .await;
        fixture.report_registration(scenario, &result);
        let mut registration = result.unwrap().unwrap();
        assert!(registered);
        assert!(registration.ready.1.is_empty());
        assert_eq!(registration.ready.2, 0);
        registration
            .require_allow_access(&metadata.allow)
            .await
            .unwrap();
        assert_eq!(registration.repository_events, enabled);
        assert_eq!(registration.git_tracking, git_tracking);
        if enabled {
            write_frame(
                &mut registration.writer,
                &Frame::RepositoryEvent {
                    event: crate::wire::repository_events::Event::PullRequestOpened {
                        remote: "origin".into(),
                        number: 7,
                    },
                },
            )
            .await
            .unwrap();
            let reply = timeout(READY_LIMIT, read_frame(&mut registration.reader))
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(
                reply,
                Some(Frame::Reply {
                    id: 7,
                    result: Ok(Value::Bool(true))
                })
            ));
        }
        close(registration).await;
    }
}

#[tokio::test]
async fn legacy_account_format_rejection_is_permanent_and_reaps_the_peer() {
    let fixture = Fixture::new();
    let mut registered = false;
    let (result, attempts) = fixture
        .register(
            "legacy-account-allow",
            &metadata(true),
            &mut registered,
            READY_LIMIT,
        )
        .await;
    let error = registration_error(result);
    assert_eq!(attempts, 1);
    assert!(!registered);
    assert_eq!(error.to_string(), relay::LEGACY_ACCOUNT_ALLOW);
    assert!(error.downcast_ref::<relay::PermanentFailure>().is_some());
    let (result, _) = fixture
        .register(
            "session-access",
            &metadata(true),
            &mut registered,
            READY_LIMIT,
        )
        .await;
    close(result.unwrap().unwrap()).await;
}

#[tokio::test]
async fn transport_spawn_failure_identifies_the_path_without_retrying() {
    let fixture = Fixture::new();
    for (kind, label) in [
        (TransportKind::Relay, "Relay transport"),
        (TransportKind::Local, "Local transport"),
    ] {
        let mut registered = false;
        let attempts = Cell::new(0usize);
        let error = registration_error(
            register_transport(
                kind,
                None,
                &metadata(false),
                &mut registered,
                || {
                    attempts.set(attempts.get() + 1);
                    Command::new(fixture.directory.join("missing-transport-executable"))
                },
                std::future::pending(),
                READY_LIMIT,
            )
            .await,
        );
        assert!(
            error
                .to_string()
                .starts_with(&format!("could not start {label} process: "))
        );
        assert!(!error.to_string().contains("OpenSSH"));
        assert_eq!(attempts.get(), 1);
        assert!(!registered);
    }
}

#[test]
fn worker_activity_frame_rejects_duplicate_and_malformed_members() {
    let member = crate::wire::WorkerMemberActivity {
        agent_id: format!("agent_{}", "a".repeat(32)),
        tool_calls_10m: 4,
        calls_per_minute: None,
        offline: false,
    };
    assert!(apply_worker_activity(vec![member.clone(), member.clone()]).is_err());
    assert!(
        apply_worker_activity(vec![member.clone(); crate::wire::MAX_COLLAB_MEMBERS + 1]).is_err()
    );
    assert!(
        apply_worker_activity(vec![crate::wire::WorkerMemberActivity {
            agent_id: "station-0".into(),
            ..member
        }])
        .is_err()
    );
}
