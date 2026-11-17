//! Registration-only tests with real, isolated fake SSH subprocesses.
//! No backend, installed coding harness, network, credentials, or model is used.
use super::*;
use serde_json::Value;
use std::{cell::Cell, fs, path::Path};

const PEER: &str = r#"
import json, os, pathlib, sys, time
root = pathlib.Path(os.environ["SSMCP_PROTOCOL_FIXTURE"])
scenario = os.environ["SSMCP_PROTOCOL_SCENARIO"]
# A second subprocess cannot register while its rejected predecessor is alive.
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
    # Force the client to terminate and reap us before trying another version.
    time.sleep(60)
    raise SystemExit(2)

unsupported = "unsupported ssmcp wire protocol"
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
if scenario == "reject-all" or (scenario.startswith("legacy") and hello["version"] != 4):
    reject(unsupported)

if scenario == "code-only" and hello["version"] > 6:
    reject(unsupported)
if scenario == "controller-only" and hello["version"] > 5:
    reject(unsupported + "; update ssmcpd and ssmcp to compatible versions (server accepts v4 and v5)")

if hello["version"] == 4:
    assert set(hello["metadata"]) <= {"name", "cwd", "hostname", "allow"}
github = bool(hello["metadata"].get("allow"))
code, ttl = ("", 0) if github else ("a" * 64, 600)
if scenario == "legacy-policy-login":
    code, ttl = "a" * 64, 0
elif scenario == "legacy-policy-ttl":
    code, ttl = "", 600
elif scenario == "legacy-no-login":
    code, ttl = "", 600
elif scenario == "legacy-no-expiry":
    code, ttl = "a" * 64, 0
print(json.dumps({"type":"ready", "url":"https://fixture.invalid/mcp",
                  "login_code":code, "login_expires_in":ttl, "client_id":"b"*64}), flush=True)
# Keep the registration live until its test explicitly performs normal cleanup.
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
        protocol: &mut u32,
        registered: &mut bool,
        deadline: Duration,
    ) -> (crate::Result<Option<RegisteredSsh>>, usize) {
        let count = Cell::new(0usize);
        let result = register_ssh(
            metadata,
            protocol,
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
            vec!["octocat@github".into()]
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
async fn current_protocol_is_preferred_without_changing_metadata() {
    let fixture = Fixture::new();
    let metadata = metadata(false);
    let mut protocol = VERSION;
    let mut registered = false;
    let (result, attempts) = fixture
        .register(
            "current",
            &metadata,
            &mut protocol,
            &mut registered,
            READY_LIMIT,
        )
        .await;
    let registration = result.unwrap().expect("registration was cancelled");
    assert_eq!(attempts, 1);
    assert_eq!(protocol, VERSION);
    assert!(registered);
    assert_eq!(fixture.hellos()[0]["version"], VERSION);
    assert_eq!(
        fixture.hellos()[0]["metadata"],
        serde_json::to_value(&metadata).unwrap()
    );
    close(registration).await;
}

#[tokio::test]
async fn explicit_rejections_use_fresh_transports_and_remember_the_legacy_version() {
    let fixture = Fixture::new();
    let metadata = metadata(true);
    let original = metadata.clone();
    let mut protocol = VERSION;
    let mut registered = false;
    let (result, attempts) = fixture
        .register(
            "legacy",
            &metadata,
            &mut protocol,
            &mut registered,
            READY_LIMIT,
        )
        .await;
    let registration = result.unwrap().expect("registration was cancelled");
    assert_eq!(attempts, 4);
    assert_eq!(protocol, LEGACY_VERSION);
    assert!(registered);
    assert!(registration.ready.1.is_empty());
    assert_eq!(registration.ready.2, 0);
    assert_eq!(
        metadata, original,
        "compatibility must not alter local task/access metadata"
    );
    let hellos = fixture.hellos();
    assert_eq!(hellos[0]["version"], VERSION);
    assert_eq!(
        hellos[0]["metadata"]["title"],
        original.title.as_ref().unwrap().as_str()
    );
    assert_eq!(hellos[1]["version"], CODE_VERSION);
    assert_eq!(
        hellos[1]["metadata"]["title"],
        original.title.as_ref().unwrap().as_str()
    );
    assert_eq!(hellos[2]["version"], CONTROLLER_VERSION);
    assert_eq!(hellos[3]["version"], LEGACY_VERSION);
    assert!(hellos[3]["metadata"].get("title").is_none());
    assert_eq!(
        hellos[3]["metadata"]["allow"],
        serde_json::to_value(&original.allow).unwrap()
    );
    close(registration).await;

    // A later connection in the same runner reuses v4 without another probe.
    let (result, attempts) = fixture
        .register(
            "legacy",
            &metadata,
            &mut protocol,
            &mut registered,
            READY_LIMIT,
        )
        .await;
    assert_eq!(attempts, 1);
    let registration = result.unwrap().expect("registration was cancelled");
    assert_eq!(
        fixture
            .hellos()
            .iter()
            .map(|value| value["version"].as_u64().unwrap())
            .collect::<Vec<_>>(),
        [7, 6, 5, 4, 4]
    );
    close(registration).await;
}

#[tokio::test]
async fn version_six_gateway_keeps_code_audit_without_sending_steering_frames() {
    let fixture = Fixture::new();
    let metadata = metadata(true);
    let mut protocol = VERSION;
    let mut registered = false;
    let (result, attempts) = fixture
        .register(
            "code-only",
            &metadata,
            &mut protocol,
            &mut registered,
            READY_LIMIT,
        )
        .await;
    let registration = result.unwrap().expect("registration was cancelled");
    assert_eq!(attempts, 2);
    assert_eq!(protocol, CODE_VERSION);
    assert!(registered);
    assert_eq!(
        fixture.hellos()[1]["metadata"],
        serde_json::to_value(&metadata).unwrap()
    );
    close(registration).await;
}

#[tokio::test]
async fn known_version_five_gateway_rejection_preserves_controller_protocol_and_title() {
    let fixture = Fixture::new();
    let metadata = metadata(true);
    let mut protocol = VERSION;
    let mut registered = false;
    let (result, attempts) = fixture
        .register(
            "controller-only",
            &metadata,
            &mut protocol,
            &mut registered,
            READY_LIMIT,
        )
        .await;
    let registration = result.unwrap().expect("registration was cancelled");
    assert_eq!(attempts, 3);
    assert_eq!(protocol, CONTROLLER_VERSION);
    assert!(registered);
    let hellos = fixture.hellos();
    assert_eq!(hellos[0]["version"], VERSION);
    assert_eq!(hellos[1]["version"], CODE_VERSION);
    assert_eq!(hellos[2]["version"], CONTROLLER_VERSION);
    assert_eq!(
        hellos[2]["metadata"]["title"],
        metadata.title.as_ref().unwrap().as_str()
    );
    close(registration).await;
}

#[tokio::test]
async fn explicit_legacy_rejection_is_terminal_after_all_supported_versions() {
    let fixture = Fixture::new();
    let mut protocol = VERSION;
    let mut registered = false;
    let (result, attempts) = fixture
        .register(
            "reject-all",
            &metadata(false),
            &mut protocol,
            &mut registered,
            READY_LIMIT,
        )
        .await;
    let error = registration_error(result);
    assert!(error.downcast_ref::<UnsupportedWireProtocol>().is_some());
    assert_eq!(attempts, 4);
    assert_eq!(protocol, LEGACY_VERSION);
    assert!(!registered);
    assert_eq!(fixture.hellos().len(), 4);
}

#[tokio::test]
async fn authentication_errors_and_untyped_failures_never_trigger_downgrade() {
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
        let mut protocol = VERSION;
        let mut registered = false;
        let (result, attempts) = fixture
            .register(
                scenario,
                &metadata(true),
                &mut protocol,
                &mut registered,
                READY_LIMIT,
            )
            .await;
        let error = registration_error(result);
        assert_eq!(attempts, 1, "{scenario}: {error}");
        assert_eq!(protocol, VERSION, "{scenario}: {error}");
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
async fn compatibility_preserves_both_allow_checks_and_login_code_requirement() {
    for (scenario, allow, expected) in [
        (
            "legacy-policy-login",
            true,
            "did not acknowledge the --allow",
        ),
        ("legacy-policy-ttl", true, "did not acknowledge the --allow"),
        ("legacy-no-login", false, "returned no login code"),
        ("legacy-no-expiry", false, "returned no login code"),
    ] {
        let fixture = Fixture::new();
        let metadata = metadata(allow);
        let original = metadata.clone();
        let mut protocol = VERSION;
        let mut registered = false;
        let (result, attempts) = fixture
            .register(
                scenario,
                &metadata,
                &mut protocol,
                &mut registered,
                READY_LIMIT,
            )
            .await;
        let error = registration_error(result);
        assert!(error.to_string().contains(expected), "{scenario}: {error}");
        assert_eq!(attempts, 4);
        assert_eq!(protocol, LEGACY_VERSION);
        assert!(
            registered,
            "a received Ready still suppresses unrelated onboarding"
        );
        assert_eq!(metadata, original);
        let hello = fixture.hellos().pop().unwrap();
        assert_eq!(
            hello["metadata"]
                .get("allow")
                .cloned()
                .unwrap_or_else(|| json!([])),
            json!(original.allow)
        );
        assert!(hello["metadata"].get("title").is_none());
    }
}

#[tokio::test]
async fn handshake_deadline_never_selects_a_legacy_protocol() {
    let fixture = Fixture::new();
    let mut protocol = VERSION;
    let mut registered = false;
    // Only this dedicated timeout case has a short injected deadline. Successful
    // process/handshake tests retain a separate generous startup allowance.
    let (result, attempts) = fixture
        .register(
            "timeout",
            &metadata(false),
            &mut protocol,
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
    assert_eq!(protocol, VERSION);
    assert!(!registered);
}
