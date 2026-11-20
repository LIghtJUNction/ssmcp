//! Registration-only tests with real, isolated fake SSH subprocesses.
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
if scenario.startswith("compat-"):
    code, ttl = "", 0
if scenario == "compat-login":
    code = "a" * 64
if scenario == "compat-ttl":
    ttl = 600
capabilities = []
if scenario in ("session-access", "compat-delegation"):
    capabilities = [os.environ["SSMCP_PROTOCOL_ACCESS_CAPABILITY"]]
elif scenario == "compat-attribution":
    capabilities = ["control_attribution_v1"]
elif scenario in ("messages", "compat-messages"):
    capabilities = [os.environ["SSMCP_PROTOCOL_MESSAGES_CAPABILITY"]]
elif scenario == "session-messages":
    capabilities = [os.environ["SSMCP_PROTOCOL_ACCESS_CAPABILITY"],
                    "control_attribution_v1",
                    os.environ["SSMCP_PROTOCOL_MESSAGES_CAPABILITY"]]
elif scenario == "compat-attribution-messages":
    capabilities = ["control_attribution_v1",
                    os.environ["SSMCP_PROTOCOL_MESSAGES_CAPABILITY"]]
elif scenario == "compat-unknown":
    capabilities = ["unknown_owner_permission"]
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
        let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
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
    ) -> (crate::Result<Option<RegisteredSsh>>, usize) {
        let count = Cell::new(0usize);
        let result = register_ssh(
            metadata,
            false,
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

    fn report_registration(&self, scenario: &str, result: &crate::Result<Option<RegisteredSsh>>) {
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

fn registration_error(result: crate::Result<Option<RegisteredSsh>>) -> crate::Error {
    match result {
        Err(error) => error,
        Ok(_) => panic!("registration unexpectedly succeeded"),
    }
}

async fn close(mut registration: RegisteredSsh) {
    stop_registration(&mut registration.ssh, &mut registration.stderr_pump)
        .await
        .unwrap();
    assert!(registration.ssh.id().is_none(), "SSH child was not reaped");
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
        registration.ssh.id().is_none(),
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
    assert!(
        error.to_string().contains("SSH authentication timed out"),
        "{error}"
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
    let result = register_ssh(
        &metadata,
        false,
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
async fn account_compat_accepts_only_self_owned_ready_and_reaps_rejected_peers() {
    for (scenario, valid) in [
        ("compat-self", true),
        ("compat-attribution", true),
        ("current", false),
        ("compat-login", false),
        ("compat-ttl", false),
        ("compat-delegation", false),
        ("compat-messages", false),
        ("compat-attribution-messages", false),
        ("compat-unknown", false),
    ] {
        let fixture = Fixture::new();
        let metadata = metadata(false);
        let mut registered = false;
        let result = register_ssh(
            &metadata,
            true,
            &mut registered,
            || fixture.command(scenario),
            std::future::pending(),
            READY_LIMIT,
        )
        .await;
        fixture.report_registration(scenario, &result);
        assert_eq!(fixture.hellos().len(), 1);
        assert!(fixture.hellos()[0]["metadata"].get("allow").is_none());
        if valid {
            let mut registration = result.unwrap().unwrap();
            assert!(!registration.access_supported);
            assert!(!registration.messaging_supported);
            assert!(registration.ready.1.is_empty());
            assert_eq!(registration.ready.2, 0);
            registration
                .require_allow_access(&metadata.allow)
                .await
                .unwrap();
            close(registration).await;
        } else {
            let error = registration_error(result);
            assert!(
                error
                    .to_string()
                    .contains("self-only account compatibility")
            );
            assert!(error.downcast_ref::<relay::PermanentFailure>().is_some());
            // The next peer takes the same exclusive lease, proving the rejected
            // process was terminated and reaped before another connection starts.
            let registration = register_ssh(
                &metadata,
                true,
                &mut registered,
                || fixture.command("compat-self"),
                std::future::pending(),
                READY_LIMIT,
            )
            .await;
            fixture.report_registration("compat-self", &registration);
            let registration = registration.unwrap().unwrap();
            close(registration).await;
        }
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
