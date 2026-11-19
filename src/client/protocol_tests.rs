//! Registration-only tests with real, isolated fake SSH subprocesses.
//! No backend, installed coding harness, network, credentials, or model is used.
use super::*;
use serde_json::Value;
use std::{cell::Cell, fs, path::Path, process::Stdio};

const PEER: &str = r#"
import json, os, pathlib, sys, time
root = pathlib.Path(os.environ["SSMCP_PROTOCOL_FIXTURE"])
scenario = os.environ["SSMCP_PROTOCOL_SCENARIO"]
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

hello = json.loads(sys.stdin.readline())
assert hello["type"] == "hello"
with open(root / "hellos.jsonl", "a", encoding="utf-8") as recorded:
    recorded.write(json.dumps(hello) + "\n")

def reject(message):
    print(json.dumps({"type":"reject", "message":message}), flush=True)
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
if scenario == "near-match":
    reject(unsupported + " ")
if scenario == "case-variant":
    reject("Unsupported ssmcp wire protocol")
if scenario == "stderr-only":
    print(unsupported, file=sys.stderr, flush=True)
    raise SystemExit(0)
if scenario == "eof":
    raise SystemExit(0)
if scenario == "malformed":
    print('{"type":', flush=True)
    raise SystemExit(0)
if scenario == "wrong-frame":
    print(json.dumps({"type":"reply", "id":1, "result":{"Err":unsupported}}), flush=True)
    raise SystemExit(0)
if scenario == "timeout":
    time.sleep(60)
    raise SystemExit(2)

github = bool(hello["metadata"].get("allow"))
code, ttl = ("", 0) if github else ("a" * 64, 600)
if scenario == "policy-login":
    code, ttl = "a" * 64, 0
elif scenario == "policy-ttl":
    code, ttl = "", 600
elif scenario == "no-login":
    code, ttl = "", 600
elif scenario == "no-expiry":
    code, ttl = "a" * 64, 0
print(json.dumps({"type":"ready", "url":"https://fixture.invalid/mcp",
                  "login_code":code, "login_expires_in":ttl, "client_id":"b"*64,
                  **({"capabilities":[os.environ["SSMCP_PROTOCOL_ACCESS_CAPABILITY"]]} if scenario == "session-access" else {})}), flush=True)
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
        let mut command = Command::new(&self.python);
        command
            .args(["-u", "-c", PEER])
            .env("SSMCP_PROTOCOL_FIXTURE", &self.directory)
            .env("SSMCP_PROTOCOL_SCENARIO", scenario)
            .env(
                "SSMCP_PROTOCOL_ACCESS_CAPABILITY",
                crate::wire::SESSION_ACCESS_CAPABILITY,
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
            registered,
            || {
                count.set(count.get() + 1);
                self.command(scenario)
            },
            std::future::pending(),
            deadline,
        )
        .await;
        (result, count.get())
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
        &mut registered,
        || {
            attempts.set(attempts.get() + 1);
            fixture.command("current")
        },
        std::future::ready(()),
        READY_LIMIT,
    )
    .await
    .unwrap();
    assert!(result.is_none());
    assert_eq!(attempts.get(), 0);
    assert!(!registered);
}
