#!/usr/bin/env python3
"""Exercise Linux background sessions using an existing user systemd manager.

Uses e2e.py's isolated real account/router fixture and real OAuth requests.
No user manager is installed or started, no machine configuration is changed,
and no coding Harness or model is invoked. Only randomly named test sessions
and a private temporary state directory are created. Missing prerequisites
produce status=blocked and exit 2, never a passing or silently skipped run.
Set SSMCP_E2E_FIXTURE_BINARY to the existing real-router library test executable.

Example: python3 scripts/e2e_daemon.py --bin-dir target/debug \
    --report target/acceptance/daemon.json --revision <tested-commit>
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import time

if sys.platform == "linux":
    from e2e import Harness, PUBLIC_TOOLS, wait_for
    from e2e_account_gateway import FIXTURE_ENV
else:
    Harness = object
    FIXTURE_ENV = "SSMCP_E2E_FIXTURE_BINARY"


class PrerequisiteUnavailable(RuntimeError):
    pass


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            digest.update(chunk)
    return digest.hexdigest()


def process_identity(pid):
    """PID plus kernel start time, so an exited/reused PID is never our target."""
    try:
        directory = Path(f"/proc/{int(pid)}")
        fields = (directory / "stat").read_text().rsplit(")", 1)[1].split()
        if fields[0] == "Z":
            return None
        return (int(pid), int(fields[19]), directory.stat().st_uid)
    except (FileNotFoundError, ProcessLookupError):
        return None


def send_owned_signal(identity, number):
    if identity is None or identity[0] <= 1 or identity[0] == os.getpid():
        raise AssertionError("refusing to signal an unverified process")
    if identity[2] != os.getuid() or process_identity(identity[0]) != identity:
        raise AssertionError("owned process identity changed before signalling")
    descriptor = os.pidfd_open(identity[0])
    try:
        if process_identity(identity[0]) != identity:
            raise AssertionError("owned process identity changed after opening its pidfd")
        signal.pidfd_send_signal(descriptor, number)
    finally:
        os.close(descriptor)


def preflight(binary_dir):
    if sys.platform != "linux":
        raise PrerequisiteUnavailable("this actual user-systemd test requires Linux")
    if not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
        raise PrerequisiteUnavailable("Python/Linux pidfd support is required for owned-process fault injection")
    required = ("systemctl", "systemd-run", "journalctl", "stty", "tty", "sleep", "head", "tr")
    programs = {name: shutil.which(name) for name in required}
    missing = [name for name, path in programs.items() if path is None]
    if not (binary_dir / "ssmcp").is_file():
        missing.append(str(binary_dir / "ssmcp"))
    fixture = os.environ.get(FIXTURE_ENV)
    if not fixture or not Path(fixture).is_file() or not os.access(fixture, os.X_OK):
        missing.append(FIXTURE_ENV + " (existing executable real-router library fixture)")
    if missing:
        raise PrerequisiteUnavailable("required existing programs are unavailable: " + ", ".join(missing))
    result = subprocess.run(
        [programs["systemctl"], "--user", "--no-pager", "show", "--property=Version"],
        stdin=subprocess.DEVNULL, capture_output=True, timeout=5,
    )
    if result.returncode != 0:
        raise PrerequisiteUnavailable("the existing systemd --user bus is unavailable; no manager was created")
    # Opening our own pidfd checks kernel support without sending a signal.
    descriptor = os.pidfd_open(os.getpid())
    os.close(descriptor)
    return programs


class Daemon(Harness):
    def __init__(self, directory, binary_dir, programs, delay):
        super().__init__(directory, binary_dir)
        self.programs = programs
        self.delay = delay
        self.workspace = directory / "workspace"
        self.state_home = directory / "sessions"
        self.workspace.mkdir(mode=0o700)
        self.state_home.mkdir(mode=0o700)
        suffix = f"{os.getpid()}-{os.urandom(5).hex()}"
        self.names = [f"daemon-e2e-{suffix}", f"daemon-e2e-peer-{suffix}"]
        self.managed = set()
        self.statuses = {}
        self.runner_pids = set()
        self.command_output = []
        self.metrics = {}
        self.probe_job_id = None
        self.live_log_snapshots = {}
        self.case = "setup"
        self.case_checks = {}
        self.cleanup_errors = []
        self.journal_since = int(time.time()) - 1
        self.environment = self.client_environment("daemon-fixture")
        self.environment["SSMCP_SESSION_HOME"] = str(self.state_home)
        self.environment.pop("SSMCP_UI_CONFIG", None)
        fixture_bin = Path(self.environment["PATH"])
        for name in ("systemctl", "systemd-run", "journalctl"):
            (fixture_bin / name).symlink_to(Path(programs[name]).resolve())
        self.launch_source = directory / "disposable-source" / "ssmcp"
        self.launch_source.parent.mkdir(mode=0o700)
        shutil.copy2(self.binary_dir / "ssmcp", self.launch_source)
        self.image_sha = sha256(self.launch_source)

    def check(self, condition, description):
        super().check(condition, description)
        self.case_checks.setdefault(self.case, []).append(description)

    def redact(self, value):
        for secret in [self.secret, *self.sensitive]:
            if secret:
                value = value.replace(secret, "[redacted]")
        return value

    def command(self, arguments, *, environment=None, executable=None, expected=0, private=False):
        process = subprocess.Popen(
            [str(executable or self.binary_dir / "ssmcp"), *arguments],
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            cwd=self.workspace, env=environment or self.environment, start_new_session=True,
        )
        try:
            output, error = process.communicate(timeout=60)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.communicate()
            raise AssertionError("owned CLI operation exceeded its bounded deadline") from None
        if not private:
            self.command_output.extend((output, error))
        if expected == 0 and process.returncode != 0:
            raise AssertionError(f"owned CLI {arguments[0]} returned exit {process.returncode}")
        if expected == "failure" and process.returncode == 0:
            raise AssertionError(f"invalid CLI {arguments[0]} unexpectedly succeeded")
        if private and process.returncode == 0:
            # Register the actual code before any further validation can fail.
            try:
                code = json.loads(output).get("login_code")
                if isinstance(code, str):
                    self.sensitive.append(code)
            except (ValueError, AttributeError):
                pass
        return process.returncode, output, error

    def cli(self, action, name=None, *, lines=None, private=False):
        arguments = ["sessions", action]
        if name is not None:
            arguments.append(name)
        if lines is not None:
            arguments += ["--lines", str(lines)]
        arguments.append("--json")
        _, output, _ = self.command(arguments, private=private)
        try:
            value = json.loads(output)
        except ValueError:
            raise AssertionError(f"sessions {action} did not return valid JSON") from None
        if action == "status":
            self.validate_status(name, value)
        return value

    def validate_status(self, name, value):
        if not isinstance(value, dict) or value.get("name") != name:
            raise AssertionError("session status has the wrong identity")
        if value.get("manager") != "systemd-user":
            raise AssertionError("Linux test did not use the actual user systemd manager")
        manager = value.get("manager_id", "")
        if not re.fullmatch(r"ssmcp-session-[0-9a-f]{64}", manager):
            raise AssertionError("session status has an invalid native manager identity")
        state = Path(value["state_directory"])
        if state.parent != self.state_home or state.is_symlink() or not state.is_dir():
            raise AssertionError("session state escaped the owned private directory")
        if "logs_path" in value:
            raise AssertionError("session status advertises the retired logs_path field")
        if "login_code" in json.dumps(value):
            raise AssertionError("session status unexpectedly includes login-code data")
        self.statuses[name] = value
        if value.get("manager_pid"):
            self.runner_pids.add(int(value["manager_pid"]))

    def connected(self, name, *, old_id=None, timeout=25):
        def observe():
            value = self.cli("status", name)
            if value["connection_state"] != "connected":
                return False
            connection = value.get("connection")
            if not isinstance(connection, dict) or not connection.get("client_id"):
                raise AssertionError("connected status has no observed Broker client ID")
            return value if connection["client_id"] != old_id else False
        return wait_for(observe, timeout=timeout, description="a new observed Broker connection")

    def native_status(self, name):
        value = self.statuses[name]
        result = subprocess.run([
            self.programs["systemctl"], "--user", "--no-pager", "show",
            value["manager_id"] + ".service",
            "--property=LoadState,ActiveState,SubState,MainPID,NRestarts,Transient,Restart,KillMode",
        ], capture_output=True, timeout=5)
        properties = dict(line.split("=", 1) for line in result.stdout.decode().splitlines() if "=" in line)
        if result.returncode != 0 and properties.get("LoadState") != "not-found":
            raise AssertionError("cannot independently observe the owned native unit")
        return properties

    def runner_identity(self, name):
        status = self.cli("status", name)
        native = self.native_status(name)
        pid = int(native.get("MainPID", "0"))
        identity = process_identity(pid)
        expected = Path(status["state_directory"]) / "ssmcp"
        if not identity or status["manager_pid"] != pid or identity[2] != os.getuid():
            raise AssertionError("runner PID does not match the owned native unit")
        if Path(os.readlink(f"/proc/{pid}/exe")) != expected:
            raise AssertionError("runner is not executing this session's pinned image")
        if sha256(expected) != self.image_sha:
            raise AssertionError("runner image does not match the tested executable")
        return identity

    def daemon_arguments(self, name, *, watchdog):
        arguments = ["--daemon", "--server", self.origin, "--no-setup", "--name", name,
                     "--title", "daemon lifecycle validation", "--shell", "/bin/sh"]
        if watchdog:
            arguments += ["--watchdog", "--reconnect-delay", str(self.delay)]
        else:
            arguments += ["--no-watchdog"]
        return arguments

    def create(self, name, *, watchdog, executable=None, environment=None):
        arguments = self.daemon_arguments(name, watchdog=watchdog)
        self.managed.add(name)
        _, output, _ = self.command(arguments, executable=executable, environment=environment)
        value = json.loads(output)
        self.validate_status(name, value)
        return value

    def authenticate(self, name, status):
        """Authorize the observed account connection through real provider OAuth."""
        connection = self.validate_connection_record(status["connection"])
        self.client_id = connection["client_id"]
        self.check(not status["auth_available"]
                   and not (Path(status["state_directory"]) / "auth.json").exists(),
                   "an account-managed session exposes no legacy login code or auth file")
        self.command(["sessions", "auth", name, "--json"], expected="failure", private=True)
        self.check(True, "sessions auth cannot mint a legacy connection code for an account relay")
        self.oauth_login(connection)
        inventory = self.public_call("list")["clients"]
        selected = [client for client in inventory if client["id"] == self.client_id]
        self.check(len(selected) == 1 and selected[0]["name"] == name
                   and selected[0]["cwd"] == str(self.workspace)
                   and selected[0].get("title") == "daemon lifecycle validation",
                   "authorized list observes the daemon name, title, workspace and current client ID")

    def verify_private_state(self, name):
        status = self.cli("status", name)
        directory = Path(status["state_directory"])
        auth_home = Path(self.environment["SSMCP_AUTH_HOME"])
        account = auth_home / (hashlib.sha256(self.origin.encode()).hexdigest() + ".json")
        paths = [self.state_home, directory, directory / "manifest.json", directory / "state.json",
                 directory / "ssmcp", auth_home, account, auth_home / "default"]
        self.check(all(path.lstat().st_uid == os.getuid() and not path.is_symlink()
                       and not stat.S_IMODE(path.stat().st_mode) & 0o077 for path in paths),
                   "saved executable, management state and account login are owned and private")
        self.check(not any(directory.glob("lifecycle*.jsonl")),
                   "the fresh private session creates no automatic lifecycle log files")
        saved = json.loads(account.read_text())
        self.check(saved.get("server") == self.origin and saved.get("token") == self.account_token,
                   "the daemon uses only the owned fixture account credential")
        self.check(status["executable_sha256"] == self.image_sha
                   and sha256(directory / "ssmcp") == self.image_sha,
                   "saved executable SHA-256 matches the tested source image")

    def probe_tools(self):
        http, value, _ = self.rpc("tools/list", modern=True)
        names = [tool["name"] for tool in value["result"]["tools"]]
        self.check(http == 200 and len(names) == len(PUBLIC_TOOLS) and set(names) == PUBLIC_TOOLS,
                   "daemon mode publishes the exact current public tool catalog, including link")
        catalog = self.public_call("list", {"client_id": self.client_id})
        self.check(all(not tool["name"].startswith(("codex.", "claude.", "pi.", "opencode."))
                       for tool in catalog["tools"]), "the fixture discovers no installed coding Harness")
        self.check(self.call("terminal.status")["running"], "the daemon retains its real interactive Shell PTY")
        self.call("terminal.write", {"data": "printf '%s%s\\n' DAEMON_PTY_ READY\n"})
        self.read_until(r"DAEMON_PTY_READY", timeout=5)
        self.check(True, "authorized MCP can use the background client's PTY")
        job = self.call("ssmcp.exec.start", {"command": [sys.executable, "-c", "print('DAEMON_EXEC_READY')"],
                                            "timeout_ms": 5000})
        self.probe_job_id = job["job_id"]
        result = self.call("ssmcp.exec.wait", {"job_id": job["job_id"], "wait_ms": 5000})
        output = self.call("ssmcp.exec.read", {"job_id": job["job_id"], "max_bytes": 4096})
        self.check(not result["running"] and result["exit_code"] == 0
                   and "DAEMON_EXEC_READY" in output["stdout"], "a real authorized daemon command completes with its observed output and exit code")

    def task_tree(self, label, *, pty=False):
        ready = self.workspace / f"{label}.json"
        source = ("import json,os,subprocess,sys,time;from pathlib import Path;"
                  "child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(120)']);"
                  "Path(sys.argv[1]).write_text(json.dumps({'parent':os.getpid(),'child':child.pid}));"
                  "print('DAEMON_TREE_READY',flush=True);time.sleep(120)")
        command = [sys.executable, "-c", source, str(ready)]
        if pty:
            self.call("terminal.write", {"data": shlex.join(command) + "\n"})
            job = None
        else:
            job = self.call("ssmcp.exec.start", {"command": command, "timeout_ms": 120000})
        def observe_tree():
            try:
                return json.loads(ready.read_text())
            except (FileNotFoundError, ValueError):
                return False
        values = wait_for(observe_tree, description="owned background task tree")
        identities = [process_identity(int(values[key])) for key in ("parent", "child")]
        if job:
            identities.append(process_identity(job["pid"]))
        self.check(all(identity and identity[2] == os.getuid() for identity in identities),
                   "owned daemon task records its actual live process tree")
        return job, identities

    def capture_live_logs(self, name, *, stdout_job_id=None):
        logs = self.cli("logs", name, lines=1000)
        self.check(isinstance(logs, dict) and logs.get("source") == "live"
                   and logs.get("storage") == "memory"
                   and isinstance(logs.get("generation"), str) and bool(logs["generation"])
                   and isinstance(logs.get("events"), list) and len(logs["events"]) <= 1000
                   and logs.get("limit") == 1000 and isinstance(logs.get("truncated"), bool),
                   "sessions logs returns a bounded snapshot of the live process memory")
        self.check(all(isinstance(row, dict) and type(row.get("sequence")) is int
                       for row in logs["events"]),
                   "live logs preserve the real attach-frame sequence identifiers")
        if stdout_job_id is not None:
            stdout = []
            for row in logs["events"]:
                event = row.get("value")
                if (row.get("kind") == "event" and isinstance(event, dict)
                        and event.get("type") == "exec.stdout"
                        and event.get("job_id") == stdout_job_id):
                    stdout.append(base64.b64decode(event["params"]["deltaBase64"], validate=True))
            self.check(b"DAEMON_EXEC_READY\n" in b"".join(stdout),
                       "the live memory snapshot contains the actual probe job's stdout event")
        self.live_log_snapshots.setdefault(name, []).append(logs)
        return logs

    def assert_live_logs_unavailable(self, name):
        _, output, error = self.command(
            ["sessions", "logs", name, "--lines", "1000", "--json"], expected="failure")
        self.check(b"live_logs_unavailable" in error,
                   "stopped sessions report live_logs_unavailable without a file or journal fallback")

    def no_credentials_in_logs(self):
        self.check(set(self.live_log_snapshots) == set(self.statuses),
                   "every owned session had its actual live log snapshot captured before shutdown")
        for name in self.managed:
            if name not in self.statuses:
                continue
            status = self.cli("status", name)
            if status["manager_active"]:
                self.capture_live_logs(name)
            else:
                self.assert_live_logs_unavailable(name)
            for logs in self.live_log_snapshots[name]:
                self.command_output.append(json.dumps(logs).encode())
            directory = Path(status["state_directory"])
            self.check(not any(directory.glob("lifecycle*.jsonl")),
                       "the owned session never creates automatic lifecycle log files")
            management = [(directory / filename).read_bytes()
                          for filename in ("state.json", "manifest.json")]
            self.command_output.extend(management)
            self.check(all(marker not in raw for raw in management
                           for marker in (b"DAEMON_EXEC_READY", b"DAEMON_PTY_READY", b"DAEMON_TREE_READY")),
                       "management state and configuration contain none of the observed task output bodies")
        self.metrics["live_log_snapshots"] = {
            name: [{"generation": logs["generation"], "events": len(logs["events"]),
                    "truncated": logs["truncated"]} for logs in snapshots]
            for name, snapshots in self.live_log_snapshots.items()
        }
        # This independent audit reads the real service journal directly. It is
        # never a fallback for the authenticated sessions logs command above.
        units = [value["manager_id"] + ".service" for value in self.statuses.values()]
        command = [self.programs["journalctl"], "--user", "--no-pager", "--output=json", "--since", f"@{self.journal_since}"]
        for unit in units:
            command += ["--unit", unit]
        result = subprocess.run(command, capture_output=True, timeout=10)
        self.check(result.returncode == 0, "the actual user journal is readable for the owned service units")
        messages = []
        for line in result.stdout.splitlines():
            record = json.loads(line)
            if record.get("_PID", "").isdigit() and int(record["_PID"]) in self.runner_pids:
                message = record.get("MESSAGE", "")
                if isinstance(message, str):
                    messages.append(message)
        self.check(bool(messages), "the user journal contains actual runner log records")
        self.check(all("\x1b" not in message for message in messages),
                   "background journal output has no terminal escape sequences")
        for message in messages:
            self.command_output.append(message.encode())
            try:
                json.loads(message)
            except ValueError:
                raise AssertionError("background runner emitted a non-JSONL journal record") from None
        self.check(True, "actual background runner journal records retain JSONL formatting")
        self.check(all(secret.encode() not in content for secret in self.sensitive if secret
                       for content in self.command_output),
                   "account and OAuth credentials are absent from ordinary status, CLI output and logs")
        self.metrics["journal_runner_records"] = len(messages)

    def watch_stopped(self, name):
        duration = self.delay + 0.75
        deadline = time.monotonic() + duration
        samples = 0
        while time.monotonic() < deadline:
            status = self.cli("status", name)
            native = self.native_status(name)
            if (status["manager_active"] or status["connection_state"] != "disconnected"
                    or not status["stop_requested"] or status["auth_available"]
                    or int(native.get("MainPID", "0")) != 0):
                raise AssertionError("an explicitly stopped session restarted during the observation window")
            samples += 1
            time.sleep(0.12)
        self.check(samples > 0, "an explicitly stopped session stays inactive throughout a window longer than its restart delay")
        self.metrics.setdefault("manual_stop_windows", []).append({"seconds": duration, "samples": samples})

    def run(self):
        self.prepare()
        primary, peer = self.names
        self.case = "D01"
        for arguments, diagnostic in (
            (["--no-watchdog", "--reconnect-delay", "2", "--server", self.origin],
             "--reconnect-delay cannot be combined with --no-watchdog"),
            (["--watchdog", "--reconnect-delay", "0", "--server", self.origin],
             "--reconnect-delay must be in 1..=3600 seconds"),
            (["--watchdog", "--reconnect-delay", "3601", "--server", self.origin],
             "--reconnect-delay must be in 1..=3600 seconds"),
            (["--daemon", "--list-tools"],
             "--list-tools cannot be combined with --daemon"),
        ):
            _, _, error = self.command(arguments, expected="failure")
            self.check(diagnostic in error.decode(errors="replace"),
                       "the real CLI rejects the specific invalid daemon/watchdog combination")
        self.check(self.cli("list") == [], "the owned session home starts empty")

        self.case = "D02"
        foreground, credentials = self.start_client("daemon-foreground-control", cwd=self.workspace)
        self.validate_connection_record(credentials)
        self.oauth_login(credentials)
        self.client_id = credentials["client_id"]
        self.check(self.call("terminal.status")["running"], "the default foreground client still starts a real PTY")
        self.stop(foreground)
        self.check(self.cli("list") == [], "a normal foreground client does not create a background session")

        self.case = "D03"
        initial = self.create(primary, watchdog=True, executable=self.launch_source)
        self.check(isinstance(initial["manager_active"], bool)
                   and initial["automatic_login_start"] is False, "daemon startup reports manager state without enabling login startup")
        first = self.connected(primary)
        self.authenticate(primary, first)
        self.probe_tools()
        self.capture_live_logs(primary, stdout_job_id=self.probe_job_id)
        native = self.native_status(primary)
        self.check(native.get("Transient") == "yes" and native.get("Restart") == "on-failure"
                   and native.get("KillMode") == "control-group", "the real native unit is transient with bounded explicit crash recovery and group cleanup")
        original_runner = self.runner_identity(primary)
        original_restarts = int(native.get("NRestarts", "0"))
        self.check(first["native_crash_restart_delay_secs"] == self.delay
                   and first["native_crash_restart_limit"] is None, "Linux reports its actual configured native crash restart policy")

        self.case = "D08"
        self.verify_private_state(primary)
        unsafe = self.directory / "unsafe-state"
        unsafe.mkdir(mode=0o755)
        unsafe.chmod(0o755)
        symlink = self.directory / "symlink-state"
        symlink.symlink_to(self.state_home, target_is_directory=True)
        for path in (unsafe, symlink):
            self.command(["sessions", "list", "--json"],
                         environment={**self.environment, "SSMCP_SESSION_HOME": str(path)}, expected="failure")
            self.check(True, "session management rejects non-private or symlink state roots")

        self.case = "D12"
        self.launch_source.unlink()
        self.launch_source.parent.rmdir()
        self.check(process_identity(original_runner[0]) == original_runner,
                   "the daemon keeps running after its owned disposable source image is removed")
        before = sha256(Path(first["state_directory"]) / "manifest.json")
        self.command(self.daemon_arguments(primary, watchdog=False), expected="failure")
        self.check(sha256(Path(first["state_directory"]) / "manifest.json") == before,
                   "duplicate daemon creation cannot overwrite a saved session definition")

        self.case = "D07"
        self.create(peer, watchdog=False)
        peer_status = self.connected(peer)
        self.capture_live_logs(peer)
        peer_runner = self.runner_identity(peer)
        self.check(peer_status["reconnect_delay_secs"] == 5 and not peer_status["watchdog"],
                   "a daemon without watchdog keeps the default delay but does not enable retries")
        entries = self.cli("list")
        self.check({entry["name"] for entry in entries} == set(self.names),
                   "sessions list independently observes both owned sessions")

        self.case = "D06"
        alpha, alpha_pids = self.task_tree("disconnect-alpha")
        beta, beta_pids = self.task_tree("disconnect-beta")
        _, pty_pids = self.task_tree("disconnect-pty", pty=True)
        self.check(alpha["job_id"] != beta["job_id"]
                   and self.call("ssmcp.exec.status", {"job_id": alpha["job_id"]})["running"]
                   and self.call("ssmcp.exec.status", {"job_id": beta["job_id"]})["running"],
                   "two independent daemon commands and its PTY task overlap in real processes")
        sibling_id = peer_status["connection"]["client_id"]
        code = "try {const r=await call(%s,'ssmcp.exec.status',{job_id:%s});return {accepted:!r.isError};} catch (_) {return {accepted:false};}" % (
            json.dumps(sibling_id), json.dumps(alpha["job_id"]))
        self.check(not self.public_call("code_mode", {"code": code})["result"]["accepted"],
                   "a sibling daemon cannot read another connection's task state")
        old_client, old_token = self.client_id, self.secret
        disconnected_at = time.monotonic()
        acknowledgement = self.account_gateway.disconnect_client(old_client)
        self.check(acknowledgement["client_id"] == old_client and acknowledgement["accepted"] is True,
                   "fault injection requests closure of exactly the owned account WebSocket connection")

        self.case = "D04"
        reconnecting = wait_for(lambda: (value if (value := self.cli("status", primary))["phase"] == "reconnecting" else False),
                                timeout=15, description="watchdog reconnect delay")
        delay_observed_at = time.monotonic()
        # The first retry adds up to 255/1024 of the configured delay and
        # reports the result rounded up to a whole second.
        delay_ms = self.delay * 1000
        max_retry_secs = (delay_ms + delay_ms * 255 // 1024 + 999) // 1000
        retry_secs = reconnecting["retry_after_secs"]
        self.metrics["reported_watchdog_retry_seconds"] = retry_secs
        self.check(reconnecting["connection_state"] != "connected" and not reconnecting["auth_available"]
                   and isinstance(retry_secs, int) and self.delay <= retry_secs <= max_retry_secs
                   and not (Path(first["state_directory"]) / "auth.json").exists(),
                   "disconnect clears the old account connection state before retrying")
        second = self.connected(primary, old_id=old_client, timeout=retry_secs + 15)
        retry_elapsed = time.monotonic() - disconnected_at
        retry_wait = time.monotonic() - delay_observed_at
        self.check(second["connection"]["client_id"] != old_client and retry_elapsed >= self.delay - 0.2,
                   "connection watchdog waits the configured delay before a replacement account connection")
        self.check(self.runner_identity(primary) == original_runner
                   and int(self.native_status(primary).get("NRestarts", "0")) == original_restarts,
                   "a WebSocket disconnect reconnects within the same service process")
        self.metrics["websocket_reconnect_seconds"] = round(retry_elapsed, 3)
        self.metrics["observed_watchdog_wait_seconds"] = round(retry_wait, 3)
        wait_for(lambda: all(process_identity(identity[0]) != identity for identity in alpha_pids + beta_pids + pty_pids),
                 timeout=10, description="old connection task/PTY descendants to exit")
        self.check(True, "WebSocket disconnect removes both old commands, their children and the owned PTY task")
        self.check(process_identity(peer_runner[0]) == peer_runner
                   and self.cli("status", peer)["connection_state"] == "connected",
                   "another background session survives the first session's disconnect")

        self.case = "D09"
        self.check(self.rpc("ping", modern=True, secret=old_token)[0] == 200,
                   "a transport disconnect preserves the account identity OAuth grant")
        self.client_id = sibling_id
        try:
            self.check(self.call("terminal.status")["running"],
                       "the existing account grant still calls the unaffected sibling connection")
        finally:
            self.client_id = old_client
        self.authenticate(primary, second)
        jobs = self.call("ssmcp.exec.list")["jobs"]
        self.check([(job["job_id"], job["mode"], job["running"]) for job in jobs] == [("main", "pty", True)],
                   "the replacement connection has only its fresh main PTY")
        stale = self.public_call("code_mode", {"code":
            "try {const r=await call(%s,'terminal.status',{});return {accepted:!r.isError};} catch (_) {return {accepted:false};}" % json.dumps(old_client)})
        self.check(not stale["result"]["accepted"], "the stale client ID cannot route a new native call")

        self.case = "D04-crash"
        _, crash_pids = self.task_tree("crash-tree")
        before_crash = self.runner_identity(primary)
        previous_id = self.client_id
        crash_at = time.monotonic()
        send_owned_signal(before_crash, signal.SIGKILL)
        observed_backoff = False
        deadline = time.monotonic() + self.delay + 20
        while time.monotonic() < deadline:
            status = self.cli("status", primary)
            pid = status.get("manager_pid")
            if pid and pid != before_crash[0]:
                break
            if pid == before_crash[0]:
                # Observe the manager's acknowledged crash transition before
                # judging its backoff state; signal delivery is asynchronous.
                time.sleep(0.04)
                continue
            if status["connection_state"] == "connected" or status["connection"] is not None:
                raise AssertionError("native crash backoff incorrectly retained the old connection")
            observed_backoff = True
            time.sleep(0.04)
        else:
            raise AssertionError("systemd did not restart the crashed owned service")
        self.check(observed_backoff and time.monotonic() - crash_at >= self.delay - 0.1,
                   "native systemd crash restart observes its configured delay")
        self.check(True, "native crash backoff never reports the previous runner as connected")
        third = self.connected(primary, old_id=previous_id)
        new_runner = self.runner_identity(primary)
        self.check(new_runner != before_crash
                   and int(self.native_status(primary).get("NRestarts", "0")) > original_restarts,
                   "the native manager starts a new service process after an actual crash")
        wait_for(lambda: all(process_identity(identity[0]) != identity for identity in crash_pids),
                 timeout=10, description="crashed runner's old command descendants")
        self.check(True, "native crash recovery removes old command descendants")
        self.metrics["native_crash_reconnect_seconds"] = round(time.monotonic() - crash_at, 3)
        self.authenticate(primary, third)

        self.case = "D05"
        stopped = self.cli("stop", primary)
        self.check(not stopped["manager_active"] and stopped["stop_requested"],
                   "manual stop confirms the native manager is inactive")
        self.watch_stopped(primary)
        self.command(["sessions", "auth", primary, "--json"], expected="failure", private=True)
        self.check(True, "explicit auth refuses a manually stopped session")
        self.cli("stop", primary)
        self.check(True, "repeating manual stop does not re-enable the watchdog")

        self.case = "D10"
        self.cli("start", primary)
        fourth = self.connected(primary, old_id=third["connection"]["client_id"])
        self.authenticate(primary, fourth)
        reloaded = self.call("ssmcp.client.reload", {})
        self.check(reloaded.get("accepted") is True, "managed reload acknowledges before replacing the connection")
        fifth = self.connected(primary, old_id=self.client_id)
        self.authenticate(primary, fifth)
        self.check(fifth["executable_sha256"] == self.image_sha
                   and len([entry for entry in self.public_call("list")["clients"] if entry["name"] == primary]) == 1,
                   "managed reload keeps the pinned image and exactly one current connection")
        before_restart = self.runner_identity(primary)
        self.cli("restart", primary)
        sixth = self.connected(primary, old_id=self.client_id)
        self.check(self.runner_identity(primary) != before_restart,
                   "explicit sessions restart replaces the native service process")
        self.authenticate(primary, sixth)

        self.case = "D11"
        unavailable_runtime = self.directory / "unavailable-user-runtime"
        unavailable_runtime.mkdir(mode=0o700)
        self.check(len(os.fsencode(unavailable_runtime / "systemd/private")) < 100,
                   "the negative manager fixture uses a short valid Unix socket pathname")
        # systemctl may use XDG_RUNTIME_DIR/systemd/private directly, bypassing
        # DBUS_SESSION_BUS_ADDRESS. Isolate both routes within an owned empty
        # directory and prove the exact native precondition before launching.
        missing_bus = {**self.environment, "XDG_RUNTIME_DIR": str(unavailable_runtime),
                       "DBUS_SESSION_BUS_ADDRESS": "unix:path=" + str(unavailable_runtime / "missing-bus")}
        unreachable = subprocess.run([
            self.programs["systemctl"], "--user", "--no-pager", "show", "--property=Version",
        ], env=missing_bus, stdin=subprocess.DEVNULL, capture_output=True, timeout=5)
        self.check(unreachable.returncode != 0,
                   "the negative fixture proves the exact native user-manager probe is unavailable")
        self.metrics["unavailable_manager_injection"] = {
            "changed_environment_keys": ["XDG_RUNTIME_DIR", "DBUS_SESSION_BUS_ADDRESS"],
            "native_probe_exit_code": unreachable.returncode,
        }
        missing_name = self.names[0] + "-unavailable"
        self.managed.add(missing_name)
        self.command(self.daemon_arguments(missing_name, watchdog=False),
                     environment=missing_bus, expected="failure")
        self.check({entry["name"] for entry in self.cli("list")} == set(self.names),
                   "an unavailable user bus leaves no half-created background session")
        self.managed.discard(missing_name)

        self.case = "D05-retry-stop"
        self.runner_identity(primary)
        current = self.cli("status", primary)
        self.account_gateway.disconnect_client(current["connection"]["client_id"])
        wait_for(lambda: self.cli("status", primary)["phase"] == "reconnecting",
                 timeout=15, description="retry-wait phase before manual stop")
        self.cli("stop", primary)
        self.watch_stopped(primary)
        self.check(process_identity(peer_runner[0]) == peer_runner,
                   "manual stop during retry leaves the other owned session alone")

        self.case = "D08-logs"
        self.no_credentials_in_logs()

        self.case = "D07-remove"
        self.cli("start", primary)
        live = self.connected(primary)
        live_pid = self.runner_identity(primary)
        directory = Path(live["state_directory"])
        removed = self.cli("remove", primary)
        self.managed.discard(primary)
        self.check(removed.get("removed") is True and not directory.exists()
                   and process_identity(live_pid[0]) != live_pid,
                   "remove stops an active owned session before deleting its saved state and executable")
        self.check([entry["name"] for entry in self.cli("list")] == [peer],
                   "removing one session preserves the other session")
        self.cli("remove", peer)
        self.managed.discard(peer)
        self.check(self.cli("list") == [], "removing both owned sessions leaves the test state home empty")

    def cleanup_sessions(self):
        for name in list(self.managed):
            try:
                directory = self.state_home / ("session-" + hashlib.sha256(name.encode()).hexdigest())
                if not directory.exists() and name not in self.statuses:
                    self.managed.discard(name)
                    continue
                self.cli("remove", name)
                self.managed.discard(name)
            except Exception as error:
                self.cleanup_errors.append(self.redact(f"{name}: {type(error).__name__}: {error}"))
        return not self.managed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--report", type=Path)
    parser.add_argument("--revision", help="source commit being validated; no git mutation or lookup is performed")
    parser.add_argument("--reconnect-delay", type=int, default=2)
    parser.add_argument("--timeout", type=int, default=240)
    arguments = parser.parse_args()
    if not 1 <= arguments.reconnect_delay <= 10:
        parser.error("test --reconnect-delay must be 1..10 seconds; the product range is tested separately")
    if not 30 <= arguments.timeout <= 1800:
        parser.error("--timeout must be 30..1800 seconds")
    if arguments.revision and not re.fullmatch(r"[0-9a-fA-F]{40}", arguments.revision):
        parser.error("--revision must be an exact 40-character commit SHA")
    started = time.monotonic()
    report = {"status": "blocked", "platform": platform.system(), "manager": "systemd-user",
              "source_revision": arguments.revision, "checks": [],
              "scope": "actual Linux user-manager lifecycle + real account WebSocket router over loopback HTTP + OAuth PKCE + MCP; no Harness/model request or installation",
              "not_covered": ["macOS launchd actual lifecycle", "Windows Task Scheduler actual lifecycle",
                              "cross-platform CI", "external GitHub service and public TLS termination", "logout/reboot startup"],
              "configured_reconnect_delay_seconds": arguments.reconnect_delay}
    harness, directory, exit_code = None, None, 1
    try:
        binary_dir = arguments.bin_dir.resolve()
        programs = preflight(binary_dir)
        report["binaries"] = {"ssmcp": sha256(binary_dir / "ssmcp"),
                              "account_router_fixture": sha256(Path(os.environ[FIXTURE_ENV]).resolve())}
        report["sources"] = {path.name: sha256(path) for path in (
            Path(__file__), Path(__file__).with_name("e2e.py"),
            Path(__file__).with_name("e2e_account_gateway.py"))}
        directory = Path(tempfile.mkdtemp(prefix="ssmcp-daemon-e2e-")).resolve()
        harness = Daemon(directory, binary_dir, programs, arguments.reconnect_delay)
        harness.fixture_timeout_seconds = min(arguments.timeout + 15, 1800)
        report["fixture_timeout_seconds"] = harness.fixture_timeout_seconds

        def interrupted(_number, _frame):
            raise TimeoutError("daemon E2E interrupted; stopping owned sessions")

        signal.signal(signal.SIGTERM, interrupted)
        signal.signal(signal.SIGALRM, interrupted)
        signal.alarm(arguments.timeout)
        report["status"] = "failed"
        harness.run()
        report["status"], exit_code = "passed", 0
    except PrerequisiteUnavailable as error:
        report["reason"] = str(error)
        exit_code = 2
    except KeyboardInterrupt:
        report["error"] = "KeyboardInterrupt: stopping owned daemon test sessions"
        exit_code = 130
    except Exception as error:
        description = f"{type(error).__name__}: {error}"
        report["status"] = "failed"
        report["error"] = harness.redact(description) if harness else description
        exit_code = 1
    finally:
        if sys.platform == "linux":
            signal.alarm(0)
        clean = True
        if harness is not None:
            clean = harness.cleanup_sessions()
            try:
                harness.cleanup()
            except Exception as error:
                clean = False
                harness.cleanup_errors.append(harness.redact(f"fixture cleanup: {type(error).__name__}: {error}"))
            report.update(checks=harness.checks, case_checks=harness.case_checks,
                          metrics=harness.metrics, cleanup_confirmed=clean,
                          cleanup_errors=harness.cleanup_errors)
            if not clean:
                report.update(status="failed", retained_private_directory=str(directory))
                exit_code = 1
        if directory is not None and clean:
            shutil.rmtree(directory)
        report["duration_seconds"] = round(time.monotonic() - started, 3)
        report["exit_code"] = exit_code
        if arguments.report:
            arguments.report.parent.mkdir(parents=True, exist_ok=True)
            arguments.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(f"Daemon validation {report['status']}: {len(report['checks'])} assertions, exit {exit_code}", flush=True)
    if report.get("reason") or report.get("error"):
        print(report.get("reason") or report.get("error"), file=sys.stderr, flush=True)
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
