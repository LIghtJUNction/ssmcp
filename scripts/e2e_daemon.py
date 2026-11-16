#!/usr/bin/env python3
"""Exercise Linux background sessions using an existing user systemd manager.

Uses e2e.py's isolated loopback SSH/Broker fixture and real OAuth requests.
No user manager is installed or started, no machine configuration is changed,
and no coding Harness or model is invoked. Only randomly named test sessions
and a private temporary state directory are created. Missing prerequisites
produce status=blocked and exit 2, never a passing or silently skipped run.

Example: python3 scripts/e2e_daemon.py --bin-dir target/debug \
    --report target/acceptance/daemon.json --revision <tested-commit>
"""

from __future__ import annotations

import argparse
import base64
import hashlib
from html.parser import HTMLParser
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
from urllib.parse import parse_qs, urlencode, urlsplit

if sys.platform == "linux":
    from e2e import Harness, PUBLIC_URL, wait_for
else:
    Harness = object
    PUBLIC_URL = "https://ssmcp.example"


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


def descendants(pid):
    result, pending = set(), [int(pid)]
    while pending:
        current = pending.pop()
        try:
            threads = list(Path(f"/proc/{current}/task").iterdir())
        except (FileNotFoundError, ProcessLookupError):
            continue
        children = []
        for thread in threads:
            try:
                children.extend((thread / "children").read_text().split())
            except (FileNotFoundError, ProcessLookupError):
                continue
        for value in children:
            child = int(value)
            if child not in result:
                result.add(child)
                pending.append(child)
    return result


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
    required = ("systemctl", "systemd-run", "journalctl", "ssh", "ssh-keygen", "stty", "tty", "sleep", "head", "tr")
    programs = {name: shutil.which(name) for name in required}
    missing = [name for name, path in programs.items() if path is None]
    sshd = shutil.which("sshd") or "/usr/sbin/sshd"
    if not Path(sshd).is_file():
        missing.append("sshd")
    missing += [str(binary_dir / name) for name in ("ssmcp", "ssmcpd")
                if not (binary_dir / name).is_file()]
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


class LoginForm(HTMLParser):
    def __init__(self):
        super().__init__()
        self.fields = {}

    def handle_starttag(self, tag, attributes):
        attributes = dict(attributes)
        if tag == "input" and attributes.get("name") and attributes.get("type") == "hidden":
            self.fields[attributes["name"]] = attributes.get("value", "")


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
        if Path(value["logs_path"]) != state / "lifecycle.jsonl":
            raise AssertionError("session logs escaped their owned state directory")
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

    def ssh_identity(self, runner):
        found = []
        expected = Path(self.programs["ssh"]).resolve()
        for pid in descendants(runner[0]):
            identity = process_identity(pid)
            if not identity or identity[2] != os.getuid():
                continue
            try:
                image = Path(os.readlink(f"/proc/{pid}/exe"))
                argv = Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")
            except (FileNotFoundError, ProcessLookupError):
                continue
            if image == expected and str(self.directory / "ssh_config").encode() in argv:
                found.append(identity)
        if len(found) > 1:
            raise AssertionError("the owned runner has overlapping fixture SSH clients")
        return found[0] if found else None

    def create(self, name, *, watchdog, executable=None, environment=None):
        arguments = ["--daemon", "--name", name, "--title", "daemon lifecycle validation",
                     "-F", str(self.directory / "ssh_config"), "-i", str(self.directory / "client"),
                     "--shell", "/bin/sh"]
        if watchdog:
            arguments += ["--watchdog", "--reconnect-delay", str(self.delay)]
        arguments.append(f"{self.user}@ssmcp-e2e")
        self.managed.add(name)
        _, output, _ = self.command(arguments, executable=executable, environment=environment)
        value = json.loads(output)
        self.validate_status(name, value)
        return value

    def authenticate(self, name, status):
        """Consume only the explicit auth response; no invented TTL field."""
        auth = self.cli("auth", name, private=True)
        code = auth.get("login_code", "")
        self.check(auth.get("name") == name and bool(re.fullmatch(r"[0-9a-f]{64}", code)),
                   "explicit sessions auth returns the actual private login code")
        self.client_id = status["connection"]["client_id"]
        resource = f"{PUBLIC_URL}/{self.user}"
        self.check(self.request(body={}, secret=code)[0] == 401,
                   "a managed-session login code is not accepted as a bearer token")
        http, server, _ = self.oauth_request("/.well-known/oauth-authorization-server")
        self.check(http == 200 and server.get("issuer") == PUBLIC_URL,
                   "managed-session authentication uses the real fixture OAuth issuer")
        if self.oauth_client is None:
            http, registration, _ = self.oauth_request(urlsplit(server["registration_endpoint"]).path,
                "POST", {"client_name": "ssmcp daemon end-to-end",
                         "redirect_uris": ["http://127.0.0.1:37401/callback"],
                         "token_endpoint_auth_method": "none",
                         "grant_types": ["authorization_code", "refresh_token"],
                         "response_types": ["code"], "application_type": "native"})
            self.check(http == 201 and registration.get("client_id"), "daemon test registers a real OAuth client")
            self.oauth_client = registration
        verifier = base64.urlsafe_b64encode(os.urandom(32)).rstrip(b"=").decode()
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
        nonce = base64.urlsafe_b64encode(os.urandom(18)).decode()
        redirect = self.oauth_client["redirect_uris"][0]
        authorize = urlsplit(server["authorization_endpoint"]).path
        fields = {"client_id": self.oauth_client["client_id"], "redirect_uri": redirect,
                  "response_type": "code", "scope": "mcp", "resource": resource, "state": nonce,
                  "code_challenge": challenge, "code_challenge_method": "S256"}
        http, html, headers = self.oauth_request(authorize + "?" + urlencode(fields), headers={"Accept": "text/html"})
        self.check(http == 200 and isinstance(html, str), "OAuth presents the managed-session login form")
        form = LoginForm()
        form.feed(html)
        self.check(bool(form.fields.get("transaction_id")), "OAuth login retains the CSRF-bound transaction")
        cookie = headers.get("set-cookie", "").split(";", 1)[0]
        http, _, headers = self.oauth_request(authorize, "POST", {**form.fields, "login_code": code},
            cookie=cookie, headers={"Origin": PUBLIC_URL, "Accept": "text/html"}, form=True)
        self.check(http == 303, "the actual managed-session code completes OAuth authorization")
        callback = urlsplit(headers["location"])
        query = parse_qs(callback.query)
        self.check(callback._replace(query="").geturl() == redirect and query.get("state") == [nonce]
                   and query.get("iss") == [PUBLIC_URL], "OAuth preserves callback, state and issuer binding")
        exchange_code = query["code"][0]
        self.sensitive.append(exchange_code)
        http, tokens, headers = self.oauth_request(urlsplit(server["token_endpoint"]).path, "POST", {
            "grant_type": "authorization_code", "client_id": self.oauth_client["client_id"],
            "code": exchange_code, "code_verifier": verifier, "redirect_uri": redirect, "resource": resource,
        }, form=True)
        if isinstance(tokens, dict):
            self.sensitive.extend(value for key in ("access_token", "refresh_token")
                                  if isinstance(value := tokens.get(key), str))
        self.check(http == 200 and tokens.get("token_type", "").lower() == "bearer"
                   and bool(tokens.get("access_token")), "OAuth exchanges the code for a real access token")
        self.secret = tokens["access_token"]
        self.check("no-store" in headers.get("cache-control", ""), "managed OAuth tokens remain non-cacheable")
        inventory = self.public_call("list")["clients"]
        selected = [client for client in inventory if client["id"] == self.client_id]
        self.check(len(selected) == 1 and selected[0]["name"] == name
                   and selected[0]["cwd"] == str(self.workspace)
                   and selected[0].get("title") == "daemon lifecycle validation",
                   "authorized list observes the daemon name, title, workspace and current client ID")

    def verify_private_state(self, name):
        status = self.cli("status", name)
        directory = Path(status["state_directory"])
        paths = [self.state_home, directory, directory / "manifest.json", directory / "state.json",
                 directory / "auth.json", directory / "lifecycle.jsonl", directory / "ssmcp"]
        self.check(all(path.lstat().st_uid == os.getuid() and not path.is_symlink()
                       and not stat.S_IMODE(path.stat().st_mode) & 0o077 for path in paths),
                   "saved executable, state, lifecycle logs and auth file are owned and private")
        self.check(status["executable_sha256"] == self.image_sha
                   and sha256(directory / "ssmcp") == self.image_sha,
                   "saved executable SHA-256 matches the tested source image")

    def probe_tools(self):
        http, value, _ = self.rpc("tools/list", modern=True)
        self.check(http == 200 and [tool["name"] for tool in value["result"]["tools"]] == ["list", "code_mode"],
                   "daemon mode still publishes exactly list and code_mode")
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

    def no_credentials_in_logs(self):
        for name in self.managed:
            if name not in self.statuses:
                continue
            logs = self.cli("logs", name, lines=1000)
            self.check(isinstance(logs, dict) and logs.get("source") == "journal"
                       and logs.get("fallback") is False and isinstance(logs.get("events"), list)
                       and logs.get("coverage") == "native_events" and logs.get("warning") is None
                       and logs.get("limit") == 1000 and isinstance(logs.get("truncated"), bool)
                       and logs.get("unit") == self.statuses[name]["manager_id"] + ".service",
                       "Linux sessions logs identifies its actual native journal source")
            if name == self.names[0]:
                stdout = []
                for row in logs["events"]:
                    event = row.get("event")
                    if (isinstance(event, dict) and event.get("type") == "exec.stdout"
                            and event.get("job_id") == self.probe_job_id):
                        stdout.append(base64.b64decode(event["params"]["deltaBase64"], validate=True))
                self.check(b"DAEMON_EXEC_READY\n" in b"".join(stdout),
                           "sessions logs exposes the real command stdout event, not just lifecycle or request text")
            self.command_output.append(json.dumps(logs).encode())
            directory = Path(self.statuses[name]["state_directory"])
            observations = []
            for filename in ("lifecycle.jsonl", "lifecycle.1.jsonl", "state.json", "manifest.json"):
                path = directory / filename
                if path.exists():
                    raw = path.read_bytes()
                    self.command_output.append(raw)
                    if filename.startswith("lifecycle"):
                        observations.extend(json.loads(line) for line in raw.splitlines() if line)
            self.check(observations and all(row.get("phase") in {
                "starting", "connected", "reconnecting", "disconnected", "stopped"
            } for row in observations), "private lifecycle records retain their separate, accurately labelled phases")
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
                   "login codes and OAuth credentials are absent from ordinary status, CLI output and logs")
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
        for arguments in (["--reconnect-delay", "2", "ssmcp-e2e"],
                          ["--watchdog", "--reconnect-delay", "0", "ssmcp-e2e"],
                          ["--watchdog", "--reconnect-delay", "3601", "ssmcp-e2e"],
                          ["--daemon", "--list-tools"]):
            self.command(arguments, expected="failure")
            self.check(True, "the real CLI rejects an invalid daemon/watchdog argument combination")
        self.check(self.cli("list") == [], "the owned session home starts empty")

        self.case = "D02"
        foreground, credentials = self.start_client("daemon-foreground-control", cwd=self.workspace)
        self.sensitive.append(credentials["login_code"])
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
        self.command(["--daemon", "--name", primary, "-F", str(self.directory / "ssh_config"),
                      "--shell", "/bin/sh", f"{self.user}@ssmcp-e2e"], expected="failure")
        self.check(sha256(Path(first["state_directory"]) / "manifest.json") == before,
                   "duplicate daemon creation cannot overwrite a saved session definition")

        self.case = "D07"
        self.create(peer, watchdog=False)
        peer_status = self.connected(peer)
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
        ssh = self.ssh_identity(original_runner)
        self.check(ssh is not None, "fault injection identifies exactly the owned fixture SSH process")
        killed_at = time.monotonic()
        send_owned_signal(ssh, signal.SIGKILL)

        self.case = "D04"
        reconnecting = wait_for(lambda: (value if (value := self.cli("status", primary))["phase"] == "reconnecting" else False),
                                timeout=15, description="watchdog reconnect delay")
        delay_observed_at = time.monotonic()
        self.check(reconnecting["connection_state"] != "connected" and not reconnecting["auth_available"]
                   and reconnecting["retry_after_secs"] == self.delay
                   and not (Path(first["state_directory"]) / "auth.json").exists(),
                   "disconnect clears the old connected state and private code before retrying")
        retry_ssh = wait_for(lambda: self.ssh_identity(original_runner), timeout=self.delay + 15,
                            description="a delayed replacement SSH process")
        retry_elapsed = time.monotonic() - killed_at
        retry_wait = time.monotonic() - delay_observed_at
        self.check(retry_ssh != ssh and retry_wait >= self.delay - 0.2,
                   "connection watchdog waits the configured delay before a new SSH process")
        second = self.connected(primary, old_id=old_client)
        self.check(self.runner_identity(primary) == original_runner
                   and int(self.native_status(primary).get("NRestarts", "0")) == original_restarts,
                   "an SSH disconnect reconnects within the same service process")
        self.metrics["ssh_reconnect_seconds"] = round(retry_elapsed, 3)
        self.metrics["observed_watchdog_wait_seconds"] = round(retry_wait, 3)
        wait_for(lambda: all(process_identity(identity[0]) != identity for identity in alpha_pids + beta_pids + pty_pids),
                 timeout=10, description="old connection task/PTY descendants to exit")
        self.check(True, "SSH disconnect removes both old commands, their children and the owned PTY task")
        self.check(process_identity(peer_runner[0]) == peer_runner
                   and self.cli("status", peer)["connection_state"] == "connected",
                   "another background session survives the first session's disconnect")

        self.case = "D09"
        self.check(self.request(body={}, secret=old_token)[0] == 401,
                   "disconnect revokes the old connection's login-code OAuth grant")
        self.authenticate(primary, second)
        self.check(self.call("ssmcp.exec.list")["jobs"] == [], "the replacement connection does not inherit old command state")
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
        self.command(["--daemon", "--name", missing_name, "-F", str(self.directory / "ssh_config"),
                      "--shell", "/bin/sh", f"{self.user}@ssmcp-e2e"],
                     environment=missing_bus, expected="failure")
        self.check({entry["name"] for entry in self.cli("list")} == set(self.names),
                   "an unavailable user bus leaves no half-created background session")
        self.managed.discard(missing_name)

        self.case = "D05-retry-stop"
        runner = self.runner_identity(primary)
        send_owned_signal(self.ssh_identity(runner), signal.SIGKILL)
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
              "scope": "actual Linux user-manager lifecycle + loopback SSH public keys + OAuth PKCE + MCP; no Harness/model request or installation",
              "not_covered": ["macOS launchd actual lifecycle", "Windows Task Scheduler actual lifecycle",
                              "cross-platform CI", "GitHub identity-provider login", "logout/reboot startup"],
              "configured_reconnect_delay_seconds": arguments.reconnect_delay}
    harness, directory, exit_code = None, None, 1
    try:
        binary_dir = arguments.bin_dir.resolve()
        programs = preflight(binary_dir)
        report["binaries"] = {name: sha256(binary_dir / name) for name in ("ssmcp", "ssmcpd")}
        report["sources"] = {path.name: sha256(path) for path in (Path(__file__), Path(__file__).with_name("e2e.py"))}
        directory = Path(tempfile.mkdtemp(prefix="ssmcp-daemon-e2e-")).resolve()
        harness = Daemon(directory, binary_dir, programs, arguments.reconnect_delay)

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
