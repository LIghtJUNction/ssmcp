//! Exercise a real client process replacing itself after a native reload call.
//! The SSH peer is a local fixture; no server, credentials, or model is used.
#![cfg(target_os = "linux")]

use serde_json::Value;
use std::{
    os::unix::{ffi::OsStrExt, fs::PermissionsExt, process::CommandExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{process::Command, time::timeout};

const SSH: &str = r#"#!/usr/bin/python3
import json, os, pathlib, shlex, sys, time

directory = pathlib.Path(os.environ['SSMCP_RELOAD_DIR'])
scenario = os.environ['SSMCP_RELOAD_SCENARIO']
generation = 2 if (directory / 'hello-1.json').exists() else 1

def save(name, value):
    path = directory / name
    with path.open('w') as output:
        json.dump(value, output)
        output.flush()
        os.fsync(output.fileno())

def send(identifier, operation):
    print(json.dumps({'type':'call', 'id':identifier, 'operation':operation}), flush=True)

def receive(identifier):
    line = sys.stdin.readline()
    assert line, 'client closed SSH before delivering a reply'
    reply = json.loads(line)
    assert reply['type'] == 'reply' and reply['id'] == identifier, reply
    return reply

def call(identifier, name, arguments):
    send(identifier, {'op':'call_tool', 'name':name, 'arguments':arguments})
    return receive(identifier)

def value(reply):
    result = reply['result']['Ok']
    assert not result.get('isError', False), reply
    return result.get('structuredContent', result)

hello = json.loads(sys.stdin.readline())
assert hello['type'] == 'hello', hello
save('hello-%d.json' % generation, hello)
save('ssh-args-%d.json' % generation, sys.argv[1:])
github = bool(hello['metadata'].get('allow'))
print(json.dumps({'type':'ready',
                  'url':'https://ssmcp.example/mcp' if github else 'https://ssmcp.example/alice',
                  'login_code':'' if github else 'a'*64,
                  'login_expires_in':0 if github else 600,
                  'client_id':('b' if generation == 1 else 'c')*64}), flush=True)

# A completed list response establishes that native startup has finished.
send(1, {'op':'list_tools'})
catalog = value(receive(1))
save('catalog-%d.json' % generation, catalog)
if generation == 2:
    save('second-ready.json', value(call(2, 'terminal.status', {})))
    sys.exit(0)

if scenario == 'invalid':
    replies = []
    for identifier, arguments in enumerate([{'force':True}, [], None, 'restart'], 10):
        replies.append(call(identifier, 'ssmcp.client.reload', arguments))
    save('invalid-replies.json', replies)
    save('still-running.json', value(call(20, 'terminal.status', {})))
    sys.exit(0)

if scenario == 'busy':
    listener = value(call(10, 'ssmcp.net.listen', {'port':0}))
    send(11, {'op':'call_tool', 'name':'ssmcp.net.accept',
              'arguments':{'id':listener['id'], 'wait_ms':1000}})
    time.sleep(0.05)
    send(12, {'op':'call_tool', 'name':'ssmcp.client.reload', 'arguments':{}})
    replies = {}
    for unused in range(2):
        line = sys.stdin.readline()
        assert line, 'reload disconnected an in-flight request'
        reply = json.loads(line)
        replies[str(reply['id'])] = reply
    save('busy-replies.json', replies)
    value(call(13, 'ssmcp.net.close', {'id':listener['id']}))
    save('still-running.json', value(call(14, 'terminal.status', {})))
    sys.exit(0)

save('old-shell.json', value(call(10, 'terminal.status', {})))
background = directory / 'background.py'
pidfile = directory / 'background-pid'
command = '/usr/bin/python3 ' + shlex.quote(str(background))
command += ' >/dev/null 2>&1 & printf \'%s\\n\' "$!" > ' + shlex.quote(str(pidfile)) + '\n'
value(call(11, 'terminal.write', {'data':command}))
deadline = time.monotonic() + 3
while not pidfile.exists() or not pidfile.read_text().strip() or not (directory / 'background-started').exists():
    assert time.monotonic() < deadline, 'background fixture was never started'
    time.sleep(0.005)

# Replace the installation while its old inode is still executing. A PATH
# launch through a symlink must follow the newly selected target on reload.
entry = directory / 'ssmcp-reload-client'
if scenario == 'symlink':
    staged = directory / 'updated-link'
    staged.symlink_to(directory / 'replacement')
    os.replace(staged, entry)
else:
    os.replace(directory / 'replacement', entry)
send(12, {'op':'call_tool', 'name':'ssmcp.client.reload', 'arguments':{}})
save('reload-reply.json', receive(12))
if scenario == 'uncommitted':
    # Losing SSH after the acknowledgment must not execute an uncommitted reload.
    sys.exit(0)
print(json.dumps({'type':'reload_commit', 'id':12}), flush=True)
# Remain alive until the client's reload shutdown closes this connection.
sys.stdin.read()
"#;

const REPLACEMENT: &str = r#"#!/usr/bin/python3
import json, os, pathlib, sys
directory = pathlib.Path(os.environ['SSMCP_RELOAD_DIR'])

def running(pid):
    try:
        state = pathlib.Path('/proc/%s/stat' % pid).read_text().rsplit(')', 1)[1].split()[0]
        return state != 'Z'
    except FileNotFoundError:
        return False

shell = json.loads((directory / 'old-shell.json').read_text())['pid']
background = (directory / 'background-pid').read_text().strip()
marker = {'argv':sys.argv[1:], 'cwd':os.getcwd(), 'pid':os.getpid(),
          'environment':os.environ['SSMCP_RELOAD_MARKER'],
          'old_shell_running':running(shell), 'old_background_running':running(background)}
with (directory / 'restarted.json').open('w') as output:
    json.dump(marker, output)
    output.flush()
    os.fsync(output.fileno())
# Continue into a fresh binary so a second Hello verifies actual reconnection.
os.execv(os.environ['SSMCP_RELOAD_BINARY'], [sys.argv[0], *sys.argv[1:]])
"#;

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        static SEQUENCE: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "ssmcp-reload-{}-{unique}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        for (name, source) in [("ssh", SSH), ("replacement", REPLACEMENT)] {
            let file = path.join(name);
            std::fs::write(&file, source).unwrap();
            std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        std::fs::create_dir(path.join("working directory")).unwrap();
        std::fs::write(path.join("ui.toml"), "").unwrap();
        std::fs::write(
            path.join("background.py"),
            "import os, pathlib, time\npathlib.Path(__file__).with_name('background-started').write_text(str(os.getpid()))\ntime.sleep(300)\n",
        ).unwrap();
        Self(path)
    }

    fn read(&self, name: &str) -> Value {
        let path = self.0.join(name);
        serde_json::from_slice(
            &std::fs::read(&path).unwrap_or_else(|error| {
                panic!("missing fixture result {}: {error}", path.display())
            }),
        )
        .unwrap()
    }

    async fn run(&self, scenario: &str, github: bool) -> (std::process::Output, Vec<String>, u32) {
        // A concurrent fork can briefly inherit another fixture's writable
        // executable fd before CLOEXEC, making its exec fail with ETXTBSY.
        // Keep installation-copy and process-lifecycle scenarios independent.
        static PROCESS_SCENARIO: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        let _scenario = PROCESS_SCENARIO.lock().await;
        let entry = self.0.join("ssmcp-reload-client");
        let executable = Path::new(env!("CARGO_BIN_EXE_ssmcp"));
        if scenario == "symlink" {
            let original = self.0.join("original-client");
            std::fs::copy(executable, &original).unwrap();
            std::os::unix::fs::symlink(original, &entry).unwrap();
        } else {
            std::fs::copy(executable, &entry).unwrap();
        }
        let mut arguments: Vec<String> = [
            "-t",
            "shell",
            "--shell",
            "/bin/sh",
            "--name",
            "reload fixture",
            "--title",
            "更新客户端 · 检验重载",
            "--log-format",
            "jsonl",
            "-p",
            "2222",
            "-i",
        ]
        .into_iter()
        .map(str::to_owned)
        .collect();
        arguments.push(self.0.join("identity with spaces").to_str().unwrap().into());
        arguments.extend([
            "--ui-config".into(),
            self.0.join("ui.toml").to_str().unwrap().into(),
        ]);
        if github {
            arguments.extend(["--allow".into(), "fixture@github".into()]);
        }
        arguments.push("alice@fixture".into());

        let mut paths = vec![self.0.clone()];
        paths.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut command = Command::new(if scenario == "symlink" {
            PathBuf::from("ssmcp-reload-client")
        } else {
            entry
        });
        command
            .args(&arguments)
            .current_dir(self.0.join("working directory"))
            .env("PATH", std::env::join_paths(paths).unwrap())
            .env("SSMCP_RELOAD_DIR", &self.0)
            .env("SSMCP_RELOAD_SCENARIO", scenario)
            .env("SSMCP_RELOAD_BINARY", executable)
            .env("SSMCP_RELOAD_MARKER", "preserved environment")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        if scenario == "argv0-collision" {
            let decoy = self.0.join("ssmcp-display-name");
            std::fs::write(
                &decoy,
                "#!/usr/bin/python3\nimport os, pathlib, sys\npathlib.Path(os.environ['SSMCP_RELOAD_DIR'], 'decoy-executed').write_text('wrong executable')\nsys.exit(23)\n",
            ).unwrap();
            std::fs::set_permissions(&decoy, std::fs::Permissions::from_mode(0o700)).unwrap();
            // This is exec -a: argv[0] names a different executable that really
            // exists in PATH, while the program being launched is our client.
            command.as_std_mut().arg0("ssmcp-display-name");
        }
        let child = command.spawn().unwrap();
        let pid = child.id().unwrap();
        let output = timeout(Duration::from_secs(15), child.wait_with_output())
            .await
            .expect("client reload fixture did not finish")
            .unwrap();
        if scenario != "uncommitted" {
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        (output, arguments, pid)
    }

    fn assert_reloaded(
        &self,
        output: &std::process::Output,
        arguments: &[String],
        pid: u32,
        auth: &str,
    ) {
        let reply = self.read("reload-reply.json");
        let ack = &reply["result"]["Ok"]["structuredContent"];
        assert_eq!(ack["accepted"], true, "{reply}");
        assert_eq!(ack["status"], "restarting");
        assert_eq!(ack["client_id"], "b".repeat(64));
        assert_eq!(ack["auth"], auth);

        let marker = self.read("restarted.json");
        assert_eq!(marker["argv"], serde_json::json!(arguments));
        assert_eq!(
            marker["cwd"],
            self.0.join("working directory").to_str().unwrap()
        );
        assert_eq!(marker["environment"], "preserved environment");
        assert_eq!(
            marker["pid"], pid,
            "reload must replace the original process"
        );
        assert_eq!(marker["old_shell_running"], false);
        assert_eq!(marker["old_background_running"], false);
        assert_eq!(self.read("hello-1.json"), self.read("hello-2.json"));
        assert_eq!(self.read("ssh-args-1.json"), self.read("ssh-args-2.json"));
        assert_eq!(self.read("second-ready.json")["running"], true);

        let connections: Vec<Value> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(connections.len(), 2);
        assert_eq!(connections[0]["client_id"], "b".repeat(64));
        assert_eq!(connections[1]["client_id"], "c".repeat(64));
        assert_eq!(connections[0]["title"], connections[1]["title"]);
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // A failed regression may leave a background fixture. Only signal a PID
        // whose command still includes this fixture's unique script path.
        if let Ok(pid) = std::fs::read_to_string(self.0.join("background-pid"))
            && let Ok(pid) = pid.trim().parse::<i32>()
            && let Ok(command) = std::fs::read(format!("/proc/{pid}/cmdline"))
            && command
                .split(|byte| *byte == 0)
                .any(|argument| argument == self.0.join("background.py").as_os_str().as_bytes())
        {
            // SAFETY: pid still identifies our uniquely named background script.
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn assert_rejected(reply: &Value) {
    assert!(
        reply["result"]["Err"].is_string() || reply["result"]["Ok"]["isError"] == true,
        "expected a rejected reload, got {reply}"
    );
}

#[tokio::test]
async fn reload_executes_updated_installation_and_preserves_launch_context() {
    let fixture = Fixture::new();
    let (output, arguments, pid) = fixture.run("replace", false).await;
    fixture.assert_reloaded(&output, &arguments, pid, "login_code");
}

#[tokio::test]
async fn reload_resolves_updated_symlink_from_path_and_keeps_github_acl() {
    let fixture = Fixture::new();
    let (output, arguments, pid) = fixture.run("symlink", true).await;
    fixture.assert_reloaded(&output, &arguments, pid, "github");
    assert_eq!(
        fixture.read("hello-2.json")["metadata"]["allow"],
        serde_json::json!(["fixture@github"])
    );
}

#[tokio::test]
async fn reload_ignores_an_exec_a_display_name_that_matches_another_program() {
    let fixture = Fixture::new();
    let (output, arguments, pid) = fixture.run("argv0-collision", false).await;
    fixture.assert_reloaded(&output, &arguments, pid, "login_code");
    assert!(!fixture.0.join("decoy-executed").exists());
}

#[tokio::test]
async fn reload_rejects_arguments_without_disconnect_or_restart() {
    let fixture = Fixture::new();
    fixture.run("invalid", false).await;
    for reply in fixture.read("invalid-replies.json").as_array().unwrap() {
        assert_rejected(reply);
    }
    assert_eq!(fixture.read("still-running.json")["running"], true);
    assert!(!fixture.0.join("hello-2.json").exists());
    assert!(!fixture.0.join("restarted.json").exists());
}

#[tokio::test]
async fn reload_rejects_an_inflight_call_and_allows_it_to_finish() {
    let fixture = Fixture::new();
    fixture.run("busy", false).await;
    let replies = fixture.read("busy-replies.json");
    assert_rejected(&replies["12"]);
    assert!(replies["12"].to_string().contains("busy"), "{replies}");
    assert_eq!(replies["11"]["result"]["Ok"]["isError"], false);
    assert_eq!(fixture.read("still-running.json")["running"], true);
    assert!(!fixture.0.join("hello-2.json").exists());
}

#[tokio::test]
async fn reload_does_not_exec_when_ssh_closes_before_commit() {
    let fixture = Fixture::new();
    fixture.run("uncommitted", false).await;
    let reply = fixture.read("reload-reply.json");
    assert_eq!(reply["result"]["Ok"]["structuredContent"]["accepted"], true);
    assert!(!fixture.0.join("restarted.json").exists());
    assert!(!fixture.0.join("hello-2.json").exists());
}
