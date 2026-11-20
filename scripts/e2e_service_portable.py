#!/usr/bin/env python3
"""Portable client service acceptance without native installation or a broker.

Runs a real foreground supervisor in a fresh SSMCP_SESSION_HOME. An owned
loopback HTTP fixture supplies a synthetic account and rejects every WebSocket
handshake with retryable HTTP 503. The real CLI stores the fixture login using
its platform-private credential writer. No browser, external OAuth service,
installed service, existing session or model account is used.

Usage:
    python scripts/e2e_service_portable.py --bin-dir target/debug \
        --report target/portable-service.json
"""
from __future__ import annotations

import argparse
import base64
import concurrent.futures
import configparser
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import plistlib
import re
import secrets
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import threading
import time


def wait_for(predicate, description, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {description}")


IDENTITY = {"provider": "github", "subject": "123", "login": "fixture",
            "principal": "fixture@github", "username": "portablefixture"}


class GatewayHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def reply(self, status, value):
        encoded = json.dumps(value).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        try:
            self.wfile.write(encoded)
        except (BrokenPipeError, ConnectionResetError):
            pass

    def do_GET(self):
        fixture = self.server.fixture
        if self.headers.get("Authorization") != "Bearer " + fixture.token:
            fixture.record("unexpected_requests")
            return self.reply(403, {"error": "invalid_fixture_credential"})
        if self.path == "/api/client/whoami":
            fixture.record("whoami")
            return self.reply(200, {"identity": IDENTITY})
        if self.path == "/api/client/connect":
            fixture.record("connect")
            return self.reply(503, {"error": "portable_fixture_unavailable"})
        fixture.record("unexpected_requests")
        self.reply(404, {"error": "fixture_route_not_found"})

    def do_POST(self):
        fixture = self.server.fixture
        try:
            size = int(self.headers.get("Content-Length", "0"))
            if not 0 <= size <= 4096:
                raise ValueError("invalid fixture request size")
            body = json.loads(self.rfile.read(size) or b"{}")
            if not isinstance(body, dict):
                raise ValueError("invalid fixture request object")
        except ValueError:
            fixture.record("unexpected_requests")
            return self.reply(400, {"error": "invalid_fixture_request"})
        if self.path == "/api/client/login/start":
            fixture.record("login_start")
            return self.reply(200, {
                "device_code": fixture.device_code, "user_code": "PORT-ABLE",
                "verification_uri": fixture.origin + "/device",
                "verification_uri_complete": fixture.origin + "/device?user_code=PORT-ABLE",
                "expires_in": 600, "interval": 1,
            })
        if self.path == "/api/client/login/poll" and body.get("device_code") == fixture.device_code:
            fixture.record("login_poll")
            return self.reply(200, {"status": "authorized", "access_token": fixture.token,
                                    "identity": IDENTITY, "expires_in": 3600})
        fixture.record("unexpected_requests")
        self.reply(404, {"error": "fixture_route_not_found"})


class UnavailableGateway:
    def __init__(self):
        self.token = secrets.token_hex(32)
        self.device_code = secrets.token_hex(32)
        self.lock = threading.Lock()
        self.counts = {key: 0 for key in
                       ("login_start", "login_poll", "whoami", "connect", "unexpected_requests")}
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), GatewayHandler)
        self.server.daemon_threads = True
        self.server.fixture = self
        self.origin = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def record(self, key):
        with self.lock:
            self.counts[key] += 1

    def snapshot(self):
        with self.lock:
            return dict(self.counts)

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        if self.thread.is_alive():
            raise AssertionError("owned loopback gateway did not stop")


class Portable:
    def __init__(self, binary: Path, root: Path, artifacts: Path):
        self.binary = binary.resolve()
        self.root = root
        self.workspace = root / "workspace unicode 中"
        self.workspace.mkdir()
        self.home = root / "sessions"
        # The supervisor creates its state root with the platform-private ACL.
        self.artifacts = artifacts
        self.environment = {key: value for key, value in os.environ.items()
                            if key.lower() not in ("http_proxy", "https_proxy", "all_proxy", "no_proxy")}
        self.environment.update(SSMCP_SESSION_HOME=str(self.home), NO_COLOR="1",
                                NO_PROXY="127.0.0.1,localhost,::1",
                                SSMCP_AUTH_HOME=str((root / "auth").resolve()),
                                SSMCP_CLIENT_CONFIG=str((root / "client.toml").resolve()))
        (root / "client.toml").touch(exist_ok=True)
        self.environment.pop("SSMCP_UI_CONFIG", None)
        self.environment.pop("SSH_AUTH_SOCK", None)
        self.environment.pop("SSH_AGENT_PID", None)
        self.manager = None
        self.manager_logs = []
        self.names = set()
        self.secrets = []
        self.checks = []
        self.samples = {}
        self.gateway = None

    def prepare_gateway(self):
        self.gateway = UnavailableGateway()
        self.secrets.extend((self.gateway.token, self.gateway.device_code))
        # Let the real account writer apply private Unix permissions or Windows
        # ACLs. Both device-flow endpoints belong to this synthetic local server.
        login = self.cli(["login", "--server", self.gateway.origin, "--no-browser", "--json"])
        value = json.loads(login.stdout)
        self.check(value.get("server") == self.gateway.origin and value.get("identity") == IDENTITY,
                   "explicit local fixture login stores the synthetic account using the real credential writer")
        self.check(all(secret not in login.stdout + login.stderr for secret in self.secrets),
                   "fixture login output excludes account and polling credentials")

    def check(self, condition, description):
        if not condition:
            raise AssertionError(description)
        self.checks.append(description)
        print(f"PASS {description}", flush=True)

    def redact(self, text):
        for secret in self.secrets:
            text = text.replace(secret, "[redacted]")
        return text

    def cli(self, args, *, expected=0, timeout=50):
        result = subprocess.run(
            [str(self.binary), *args],
            cwd=self.workspace, env=self.environment, stdin=subprocess.DEVNULL,
            capture_output=True, text=True, encoding="utf-8", errors="replace",
            timeout=timeout,
        )
        if expected == 0 and result.returncode != 0:
            raise AssertionError(
                f"CLI {' '.join(args[:2])} failed ({result.returncode}): "
                f"{self.redact(result.stderr[-1600:])}"
            )
        if expected == "failure" and result.returncode == 0:
            raise AssertionError(f"CLI {' '.join(args[:2])} unexpectedly succeeded")
        return result

    def action(self, operation, name):
        return json.loads(self.cli(["sessions", operation, name, "--json"]).stdout)

    def current_endpoint(self):
        path = self.home / "service.json"
        if not path.is_file():
            return None
        value = json.loads(path.read_text(encoding="utf-8"))
        if self.manager is None or value["pid"] != self.manager.pid:
            return None
        token = value["token"]
        if token not in self.secrets:
            self.secrets.append(token)
        return value

    @staticmethod
    def read_exact(connection, count):
        value = bytearray()
        while len(value) < count:
            chunk = connection.recv(count - len(value))
            if not chunk:
                raise AssertionError("local control reply ended early")
            value.extend(chunk)
        return bytes(value)

    def request(self, request, *, allow_error=False, timeout=8):
        endpoint = self.current_endpoint()
        if endpoint is None:
            raise AssertionError("owned foreground service endpoint is unavailable")
        payload = json.dumps({"token": endpoint["token"], "request": request}).encode()
        with socket.create_connection(("127.0.0.1", endpoint["port"]), timeout) as peer:
            peer.settimeout(timeout)
            peer.sendall(struct.pack(">I", len(payload)) + payload)
            length = struct.unpack(">I", self.read_exact(peer, 4))[0]
            if length == 0 or length > 256 * 1024:
                raise AssertionError("invalid bounded local control reply")
            reply = json.loads(self.read_exact(peer, length))
        if reply.get("error") is not None:
            if allow_error:
                return {"error": reply["error"]}
            raise AssertionError(self.redact(reply["error"]))
        return reply.get("result")

    def launch(self):
        log = self.root / f"manager-{len(self.manager_logs)}.log"
        self.manager_logs.append(log)
        options = {}
        if os.name == "nt":
            options["creationflags"] = subprocess.CREATE_NEW_PROCESS_GROUP
        else:
            options["start_new_session"] = True
        with log.open("wb") as stream:
            self.manager = subprocess.Popen(
                [str(self.binary), "service", "run"],
                cwd=self.workspace, env=self.environment, stdin=subprocess.DEVNULL,
                stdout=stream, stderr=stream, **options,
            )

        def ready():
            if self.manager.poll() is not None:
                raise AssertionError(
                    "foreground supervisor exited before readiness: "
                    + self.redact(log.read_text(encoding="utf-8", errors="replace")[-1600:])
                )
            if self.current_endpoint() is None:
                return False
            try:
                return self.request({"op": "ping"}) == {
                    "service": "ssmcp", "protocol": 1,
                }
            except (OSError, AssertionError):
                return False

        wait_for(ready, "owned supervisor readiness")
        self.check(True, "foreground supervisor starts and answers authenticated status")

    def stop(self, *, checked=True):
        manager = self.manager
        if manager is None or manager.poll() is not None:
            return
        if os.name == "nt":
            # The supervisor owns this new process group. CTRL_BREAK never
            # broadcasts to the test runner's own console group.
            manager.send_signal(signal.CTRL_BREAK_EVENT)
        else:
            manager.send_signal(signal.SIGTERM)
        manager.wait(timeout=35)
        if checked:
            self.check(manager.returncode == 0,
                       "foreground supervisor handles targeted shutdown successfully")
            self.check(not (self.home / "service.json").exists(),
                       "graceful shutdown removes its own control endpoint")

    def create_args(self, name):
        return [
            "sessions", "create", name, "--json", "--server", self.gateway.origin, "--watchdog",
            "--reconnect-delay", "1", "--no-setup", "--title",
            "portable lifecycle fixture; unavailable account gateway",
            "--cwd", str(self.workspace),
        ]

    def create(self, name, *, expected=0):
        self.names.add(name)
        return self.cli(self.create_args(name), expected=expected)

    def failed_connection(self, name):
        def observed():
            value = self.action("status", name)
            if value.get("connection_state") == "connected":
                raise AssertionError("unavailable-gateway fixture unexpectedly connected")
            if value.get("attempt", 0) > 0 and value.get("last_error"):
                return value
            return False

        value = wait_for(observed, f"failed loopback account connection for {name}")
        self.check(self.gateway.snapshot()["connect"] > 0,
                   "the owned gateway receives and rejects real WebSocket attempts with HTTP 503")
        self.samples.setdefault("first_connection_error", self.redact(value["last_error"]))
        self.check(value.get("supervised") and value.get("manager") == "ssmcp-service"
                   and bool(value.get("generation")) and value.get("manager_active"),
                   "saved session is owned by the live supervisor and has a generation")
        self.check("login_code" not in json.dumps(value),
                   "public status contains no login credentials")
        return value

    def definition_checks(self):
        user = json.loads(self.cli(["service", "install", "--dry-run"]).stdout)
        self.check(user.get("dry_run") and user.get("scope") == "user",
                   "default installation dry-run plans only a user registration")
        if os.name == "nt":
            account = subprocess.run(
                ["whoami"], check=True, capture_output=True, text=True,
                encoding="utf-8", errors="replace", timeout=10,
            ).stdout.strip()
        else:
            import pwd
            account = pwd.getpwuid(os.getuid()).pw_name
        system = json.loads(self.cli([
            "service", "install", "--system", "--user", account, "--dry-run",
        ]).stdout)
        self.check(system.get("dry_run") and system.get("scope") == "system"
                   and system.get("owner") == account,
                   "system dry-run records the explicit service account")

        if sys.platform == "darwin":
            for plan, is_system in [(user, False), (system, True)]:
                definition = plistlib.loads(plan["definition"].encode())
                self.check(definition["ProgramArguments"][1:] == ["service", "run"]
                           and definition["RunAtLoad"],
                           "launchd definition parses and preserves literal startup arguments")
                self.check(("UserName" in definition) == is_system,
                           "launchd user and system definitions have distinct identity scopes")
        elif sys.platform == "linux":
            for plan, is_system in [(user, False), (system, True)]:
                definition = configparser.ConfigParser(interpolation=None, strict=False)
                definition.read_string(plan["definition"])
                self.check(definition["Service"]["Type"] == "exec"
                           and definition["Service"]["Restart"] == "on-failure",
                           "systemd definition has observable startup and failure recovery")
                self.check(definition["Service"].get("User") == (account if is_system else None),
                           "systemd definition preserves the requested account scope")
                verifier = shutil.which("systemd-analyze")
                if verifier is None:
                    raise RuntimeError("systemd-analyze is required to validate generated Linux units")
                unit = self.root / ("fixture-system.service" if is_system else "fixture-user.service")
                unit.write_text(plan["definition"], encoding="utf-8")
                verified = subprocess.run(
                    [verifier, "verify", "--man=no", str(unit)],
                    cwd=self.root, env=self.environment, stdin=subprocess.DEVNULL,
                    capture_output=True, text=True, encoding="utf-8", errors="replace",
                    timeout=20,
                )
                if verified.returncode != 0:
                    raise AssertionError(
                        "systemd-analyze rejected the generated "
                        + ("system" if is_system else "user")
                        + " unit: " + self.redact(verified.stderr[-3000:])
                    )
                self.check(True, "systemd accepts the generated "
                           + ("system" if is_system else "user") + " unit definition")

        else:
            self.check(user.get("manager") == "task-scheduler"
                       and "$d.Triggers.Create(9)" in user["definition"]
                       and "$d.Principal.LogonType=3" in user["definition"],
                       "Windows user definition has an interactive-token login trigger")
            encoded = re.search(r"-EncodedCommand ([A-Za-z0-9+/=]+)", user["definition"])
            self.check(encoded is not None, "Windows task has an encoded literal worker command")
            worker = base64.b64decode(encoded.group(1)).decode("utf-16-le")
            self.check("'service' 'run'" in worker and "SSMCP_SESSION_HOME" in worker,
                       "Windows task worker selects the fixture state and foreground manager")
            self.check(system.get("manager") == "windows-scm"
                       and system.get("arguments") == ["service", "run", "--scm"]
                       and system.get("startup") == "automatic",
                       "Windows system definition uses the real SCM dispatcher")
            outside_scm = self.cli(["service", "run", "--scm"], expected="failure", timeout=15)
            self.check("connect to Windows SCM" in outside_scm.stderr,
                       "SCM dispatcher rejects an ordinary console invocation")

        self.cli(["service", "install", "--user", account, "--dry-run"], expected="failure")
        self.cli(["service", "restart", "--dry-run"], expected="failure")
        self.check(True, "ambiguous account scope and unsupported dry-run operations are rejected")
        self.check(not (self.home / "service.json").exists(),
                   "installation previews did not start a manager")

    def run(self):
        self.prepare_gateway()
        self.definition_checks()
        self.launch()
        self.check(json.loads(self.cli(["attach", "--json"]).stdout) == [],
                   "bare attach lists an empty manager without creating a session")
        initial = json.loads(self.create("portable-closed").stdout)
        self.check(initial.get("name") == "portable-closed" and initial.get("generation"),
                   "create acknowledges the named worker generation")
        original = self.failed_connection("portable-closed")
        self.create("portable-restored")
        restored = self.failed_connection("portable-restored")
        self.samples["initial_generation"] = original["generation"]

        def retried_connection():
            value = self.action("status", "portable-restored")
            return value if value.get("attempt", 0) > restored["attempt"] else False

        retried = wait_for(retried_connection, "ordinary account outage retry")
        self.check(retried["generation"] == restored["generation"],
                   "a retryable account gateway outage retains the same supervised worker generation")

        listings = [
            json.loads(self.cli(args).stdout)
            for args in [
                ["sessions", "list", "--json"],
                ["attach", "--json"],
                ["attach", "list", "--json"],
            ]
        ]
        expected = {"portable-closed", "portable-restored"}
        self.check(all({entry["name"] for entry in entries} == expected for entries in listings),
                   "session list and both attach listing forms show the same identities")
        self.cli(["attach", "--local", "absent-fixture", "--read-only"], expected="failure")
        self.check({entry["name"] for entry in json.loads(
            self.cli(["sessions", "list", "--json"]).stdout)} == expected,
            "attaching an absent name does not create it")

        self.names.add("portable-race")
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            results = list(pool.map(
                lambda _: self.cli(self.create_args("portable-race"), expected=None), range(2),
            ))
        self.check(sum(result.returncode == 0 for result in results) == 1,
                   "concurrent duplicate create commits exactly one session")
        self.action("remove", "portable-race")
        self.names.remove("portable-race")

        restarted = self.action("restart", "portable-closed")
        self.check(restarted.get("generation") != original["generation"]
                   and restarted.get("manager_active"),
                   "restart acknowledges a new active generation after old-worker shutdown")
        self.samples["restarted_generation"] = restarted["generation"]
        stale = self.request({
            "op": "close", "name": "portable-closed",
            "expected_generation": original["generation"],
        }, allow_error=True)
        self.check("generation" in stale.get("error", ""),
                   "stale generation cannot close the replacement worker")
        current = self.action("status", "portable-closed")
        self.check(current["generation"] == restarted["generation"],
                   "rejected stale close leaves the active generation unchanged")

        closed = self.action("close", "portable-closed")
        self.check(closed.get("stop_requested") and not closed.get("manager_active")
                   and closed.get("manager_pid") is None,
                   "close acknowledges worker exit and persists stopped intent")
        self.stop()
        self.launch()

        def restored_generation():
            value = self.action("status", "portable-restored")
            return value if value.get("manager_active") and value.get("generation") != restored["generation"] else False

        restored_again = wait_for(restored_generation, "desired-running session restoration")
        self.samples["restored_generation"] = restored_again["generation"]
        self.check(True, "supervisor restart restores a desired-running session as a new generation")
        # More than one supervisor tick and one configured retry interval.
        time.sleep(1.3)
        closed_again = self.action("status", "portable-closed")
        self.check(closed_again.get("stop_requested") and not closed_again.get("manager_active")
                   and closed_again["generation"] == restarted["generation"],
                   "closed watchdog session stays stopped after supervisor restart")
        started = self.action("start", "portable-closed")
        self.check(started.get("manager_active") and started["generation"] != restarted["generation"],
                   "explicit start reactivates retained closed configuration")
        self.failed_connection("portable-closed")

        for name in sorted(self.names):
            self.action("remove", name)
        self.names.clear()
        self.check(json.loads(self.cli(["attach", "--json"]).stdout) == [],
                   "remove clears only the disposable fixture sessions")
        self.stop()
        observed = self.gateway.snapshot()
        self.samples["gateway_requests"] = observed
        self.check(observed["login_start"] == observed["login_poll"] == 1
                   and observed["whoami"] > 0 and observed["connect"] > 1
                   and observed["unexpected_requests"] == 0,
                   "session retries and supervisor restoration reuse the local login without new authorization")

    def diagnostics(self):
        for index, log in enumerate(self.manager_logs):
            if log.is_file():
                value = self.redact(log.read_text(encoding="utf-8", errors="replace")[-32768:])
                (self.artifacts / f"portable-manager-{index}.log").write_text(value, encoding="utf-8")

    def cleanup(self):
        try:
            self.cleanup_manager()
        finally:
            if self.gateway is not None:
                self.samples["gateway_requests"] = self.gateway.snapshot()
                self.gateway.close()
                self.gateway = None

    def cleanup_manager(self):
        if self.manager is None or self.manager.poll() is not None:
            return
        for name in sorted(self.names):
            try:
                self.cli(["sessions", "remove", name, "--json"], expected=None, timeout=10)
            except Exception:
                pass
        try:
            self.stop(checked=False)
        except Exception:
            # This is the Popen-owned supervisor/process group, never a PID
            # obtained from a saved file or another user's session.
            if self.manager.poll() is None:
                if os.name == "nt":
                    self.manager.kill()
                else:
                    os.killpg(self.manager.pid, signal.SIGKILL)
                self.manager.wait(timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--revision", default="working-tree")
    args = parser.parse_args()
    binary = args.bin_dir.resolve() / ("ssmcp.exe" if os.name == "nt" else "ssmcp")
    report_path = args.report.resolve()
    report_path.parent.mkdir(parents=True, exist_ok=True)
    report = {
        "revision": args.revision, "platform": sys.platform, "status": "failed",
        "scope": "isolated foreground supervisor; installation previews only",
        "transport": "owned loopback account gateway; HTTP 503 before WebSocket upgrade",
        "account": "synthetic local login saved by the real client",
        "external_oauth": False, "models_started": False,
        "checks": [], "samples": {},
    }
    if sys.platform not in ("linux", "darwin", "win32") or (
        os.name != "nt" and os.geteuid() == 0
    ):
        report.update(status="blocked", error="requires a non-root Linux/macOS user or Windows user")
        report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        return 2
    if not binary.is_file():
        report.update(status="blocked", error=f"build the ssmcp binary first: {binary}")
        report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
        return 2

    with tempfile.TemporaryDirectory(prefix="ssmcp-portable-") as temporary:
        fixture = Portable(binary, Path(temporary), report_path.parent)
        try:
            fixture.run()
            report["status"] = "passed"
        except Exception as error:
            report["error"] = fixture.redact(str(error))
            fixture.diagnostics()
            print(f"FAIL {report['error']}", file=sys.stderr)
        finally:
            try:
                fixture.cleanup()
            except Exception as error:
                report["status"] = "failed"
                report["cleanup_error"] = fixture.redact(str(error))
            report["checks"] = fixture.checks
            report["samples"] = fixture.samples
            report_path.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
