#!/usr/bin/env python3
"""Real account/relay CLI integration against a local synthetic GitHub gateway.

Python standard library only. No real OAuth, SSH, user credentials or models.
"""
from __future__ import annotations
import argparse
import base64
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import secrets
import signal
import socket
import stat
import struct
import subprocess
import tempfile
import threading
import time

IDENTITY = {"provider": "github", "subject": "123", "login": "fixture",
            "principal": "fixture@github", "username": "fixture"}
CAPABILITIES = ("session_access_v1", "control_attribution_v1")
MAX_FRAME = 4 * 1024 * 1024
SUPPORTED_WIRE_VERSIONS = (4, 5, 6, 7)

def wait_for(predicate, description, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        result = predicate()
        if result:
            return result
        time.sleep(0.02)
    raise AssertionError("Timed out: " + description)

def process_running(pid):
    try:
        return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z"
    except FileNotFoundError:
        return False

def descendants(pid):
    found = []
    try:
        children = Path(f"/proc/{pid}/task/{pid}/children").read_text().split()
    except FileNotFoundError:
        return found
    for child in children:
        child = int(child)
        found.append(child)
        found.extend(descendants(child))
    return found

class WebSocket:
    def __init__(self, handler, token, state):
        self.handler, self.token, self.state = handler, token, state
        self.lock = threading.Lock()
        self.pending = {}
        self.sequence = 0
        self.id = secrets.token_hex(32)
        self.hello = None
        self.closed = threading.Event()

    def send(self, payload, opcode=2):
        if not isinstance(payload, bytes):
            payload = json.dumps(payload, separators=(",", ":")).encode() + b"\n"
        prefix = bytes([0x80 | opcode])
        length = len(payload)
        if length < 126:
            prefix += bytes([length])
        elif length < 65536:
            prefix += b"\x7e" + struct.pack("!H", length)
        else:
            prefix += b"\x7f" + struct.pack("!Q", length)
        with self.lock:
            self.handler.wfile.write(prefix + payload)
            self.handler.wfile.flush()

    def exact(self, count):
        value = self.handler.rfile.read(count)
        if len(value) != count:
            raise EOFError()
        return value

    def read(self):
        while True:
            first, second = self.exact(2)
            assert first & 0x80, "fixture expects complete frames"
            opcode = first & 0x0F
            assert second & 0x80, "client frames must be masked"
            size = second & 0x7F
            if size == 126:
                size = struct.unpack("!H", self.exact(2))[0]
            elif size == 127:
                size = struct.unpack("!Q", self.exact(8))[0]
            assert size <= MAX_FRAME, "oversized client frame"
            mask = self.exact(4)
            payload = bytes(byte ^ mask[i % 4] for i, byte in enumerate(self.exact(size)))
            if opcode == 8:
                raise EOFError()
            if opcode == 9:
                self.send(payload, 10)
                continue
            if opcode == 10:
                continue
            assert opcode in (1, 2), "unexpected websocket opcode"
            return json.loads(payload)

    def call(self, name, arguments):
        with self.state.lock:
            self.sequence += 1
            identifier = self.sequence
            result = queue.Queue(maxsize=1)
            self.pending[identifier] = result
        self.send({"type": "call", "id": identifier,
                   "operation": {"op": "call_tool", "name": name, "arguments": arguments}})
        try:
            reply = result.get(timeout=10)
        finally:
            with self.state.lock:
                self.pending.pop(identifier, None)
        if "Err" in reply:
            raise AssertionError("Native call failed: " + reply["Err"])
        value = reply["Ok"]
        if value.get("isError"):
            raise AssertionError("Native tool reported an error: " + json.dumps(value))
        return value.get("structuredContent", value)

    def close(self, code=1001):
        if not self.closed.is_set():
            try:
                self.send(struct.pack("!H", code), 8)
            except (BrokenPipeError, ConnectionResetError, OSError):
                pass

    def serve(self):
        try:
            self.hello = self.read()
            assert self.hello["type"] == "hello"
            assert self.hello["version"] in SUPPORTED_WIRE_VERSIONS
            assert self.hello["mode"] == "shell"
            assert self.hello["metadata"]["allow"] == [self.state.allow_principal]
            self.send({"type": "ready", "url": self.state.origin + "/mcp",
                       "client_id": self.id, "login_code": "", "login_expires_in": 0,
                       "capabilities": list(CAPABILITIES)})
            with self.state.lock:
                self.state.sessions[self.id] = self
                self.state.connections.setdefault(self.hello["metadata"]["name"], []).append(self)
                revoke = self.state.next_revoke_on_ready
                self.state.next_revoke_on_ready = False
            if revoke:
                self.close(4001)
            while True:
                frame = self.read()
                if frame["type"] == "access":
                    # The current client requests the relay-owned allow list at
                    # startup. This fixture supports only that read, not ACL edits.
                    assert frame["command"] == {"action": "list"}
                    self.send({"type": "access_reply", "id": frame["id"], "result": {"Ok": {
                        "entries": [{"principal": self.state.allow_principal,
                                     "login": IDENTITY["username"], "subject": IDENTITY["subject"],
                                     "owner": bool(self.token)}],
                    }}})
                    continue
                if frame["type"] == "steering":
                    # Account lifecycle tests have no user requirements. Only
                    # v7 presence polling is valid here; never simulate queue
                    # publication, editing, or response-boundary delivery.
                    assert self.hello["version"] == 7
                    assert frame["command"] == {"op": "snapshot"}
                    self.send({"type": "steering_reply", "id": frame["id"], "result": {"Ok": {
                        "schema_version": 1, "client_id": self.id, "revision": 0,
                        "agents": [], "messages": [],
                    }}})
                    continue
                assert frame["type"] == "reply", f"unexpected client frame type: {frame['type']}"
                with self.state.lock:
                    result = self.pending.get(frame["id"])
                if result is not None:
                    result.put(frame["result"])
        except (EOFError, BrokenPipeError, ConnectionResetError, socket.timeout):
            pass
        except Exception as error:
            self.state.errors.append(f"{type(error).__name__}: {error}")
        finally:
            self.closed.set()
            with self.state.lock:
                self.state.sessions.pop(self.id, None)
                for result in self.pending.values():
                    if result.empty():
                        result.put({"Err": "fixture connection closed"})

class State:
    def __init__(self):
        self.lock = threading.RLock()
        self.account_token = "cl_" + secrets.token_hex(32)
        self.device_code = secrets.token_hex(32)
        self.pending_polls = 1
        self.sessions = {}
        self.connections = {}
        self.next_revoke_on_ready = False
        self.requests = []
        self.connect_attempts = {}
        self.reject_connect = None
        self.errors = []
        self.origin = ""
        self.logged_out = False
        self.whoami_identity = None
        self.redirect_whoami = None

    @property
    def allow_principal(self):
        # HTTP is permitted only for this owned loopback fixture.
        return f"{IDENTITY['username']}@{self.origin}"

    def identity(self, token):
        return token == self.account_token and not self.logged_out

class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    def log_message(self, *_):
        pass
    @property
    def state(self):
        return self.server.state
    def bearer(self):
        return self.headers.get("Authorization", "").removeprefix("Bearer ")
    def respond(self, status, value, headers=None):
        encoded = json.dumps(value).encode()
        self.send_response(status)
        for name, value in (headers or {}).items():
            self.send_header(name, value)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(encoded)
        self.close_connection = True

    def do_GET(self):
        self.state.requests.append(("GET", self.path))
        token = self.bearer()
        if self.path == "/api/client/connect":
            self.state.connect_attempts[token] = self.state.connect_attempts.get(token, 0) + 1
            # A present invalid credential never becomes an anonymous session.
            if self.headers.get("Authorization") is not None and not self.state.identity(token):
                return self.respond(401, {"error": "invalid_token"})
            if not token and self.state.reject_connect is not None:
                return self.respond(self.state.reject_connect, {"error": "not_allowed"})
            offered = {part.strip() for part in self.headers.get("ssmcp-capabilities", "").split(",")}
            if not set(CAPABILITIES).issubset(offered):
                return self.respond(400, {"error": "anonymous_control_requires_current_client"})
            key = self.headers["Sec-WebSocket-Key"]
            accept = base64.b64encode(hashlib.sha1(
                (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
            self.send_response(101)
            self.send_header("Upgrade", "websocket")
            self.send_header("Connection", "Upgrade")
            self.send_header("Sec-WebSocket-Accept", accept)
            self.end_headers()
            self.close_connection = True
            self.connection.settimeout(20)
            WebSocket(self, token, self.state).serve()
            return
        if not self.state.identity(token):
            return self.respond(401, {"error": "invalid_token"})
        if self.path == "/api/client/whoami":
            if self.state.redirect_whoami:
                return self.respond(302, {"error": "redirect"},
                                    {"Location": self.state.redirect_whoami})
            return self.respond(200, {"identity": self.state.whoami_identity or IDENTITY})
        if self.path == "/api/client/sessions":
            with self.state.lock:
                clients = [{"id": connection.id, "type": "shell", **connection.hello["metadata"]}
                           for connection in self.state.sessions.values()
                           if self.state.allow_principal in connection.hello["metadata"]["allow"]]
            return self.respond(200, {"clients": clients})
        self.respond(404, {"error": "not_found"})

    def do_POST(self):
        self.state.requests.append(("POST", self.path))
        size = int(self.headers.get("Content-Length", "0"))
        assert 0 <= size <= 65536
        body = json.loads(self.rfile.read(size) or b"{}")
        if self.path == "/api/client/login/start":
            return self.respond(200, {
                "device_code": self.state.device_code, "user_code": "ABCD-EFGH",
                "verification_uri": self.state.origin + "/device",
                "verification_uri_complete": self.state.origin + "/device?user_code=ABCD-EFGH",
                "expires_in": 600, "interval": 1})
        if self.path == "/api/client/login/poll":
            assert body["device_code"] == self.state.device_code
            if self.state.logged_out:
                # A foreground account client may offer one new authorization.
                # The user declines it here; never mint the revoked token again.
                return self.respond(200, {"status": "denied"})
            if self.state.pending_polls:
                self.state.pending_polls -= 1
                return self.respond(200, {"status": "pending"})
            return self.respond(200, {"status": "authorized", "access_token": self.state.account_token,
                                      "identity": IDENTITY, "expires_in": 3600})
        if not self.state.identity(self.bearer()):
            return self.respond(401, {"error": "invalid_token"})
        if self.path == "/api/client/logout":
            self.state.logged_out = True
            with self.state.lock:
                owned = [peer for peer in self.state.sessions.values()
                         if peer.token == self.state.account_token]
            for peer in owned:
                peer.close(4001)
            return self.respond(200, {"logged_out": True})
        if self.path == "/api/client/call":
            with self.state.lock:
                connection = self.state.sessions.get(body["client_id"])
            if connection is None:
                return self.respond(404, {"error": "not_found"})
            try:
                value = connection.call(body["name"], body["arguments"])
            except Exception as error:
                return self.respond(502, {"error": str(error)})
            return self.respond(200, {"content": [], "structuredContent": value})
        self.respond(404, {"error": "not_found"})

class Harness:
    def __init__(self, directory, binary):
        self.root, self.binary = directory, str(binary)
        self.state = State()
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.server.state = self.state
        self.state.origin = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.foreign = State()
        self.foreign_server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.foreign_server.daemon_threads = True
        self.foreign_server.state = self.foreign
        self.foreign.origin = f"http://127.0.0.1:{self.foreign_server.server_address[1]}"
        self.foreign_thread = threading.Thread(target=self.foreign_server.serve_forever, daemon=True)
        self.foreign_thread.start()
        self.processes, self.logs, self.checks = [], [], []
        (directory / "empty-path").mkdir()
        for device in ("A", "B"):
            (directory / device).mkdir()
            (directory / device / "auth").mkdir(mode=0o700)

    def env(self, device):
        result = {key: value for key, value in os.environ.items() if not key.startswith("SSMCP_")}
        result.update(PATH=str(self.root / "empty-path"),
                      XDG_CONFIG_HOME=str(self.root / device / "config"),
                      SSMCP_AUTH_HOME=str(self.root / device / "auth"),
                      SSMCP_SESSION_HOME=str(self.root / device / "sessions"),
                      NO_COLOR="1", TERM="dumb")
        return result

    def redacted(self, text):
        for value in [self.state.account_token, self.state.device_code,
                      self.foreign.account_token, self.foreign.device_code]:
            text = text.replace(value, "[redacted]")
        return text

    def check(self, condition, description):
        if not condition:
            raise AssertionError(description)
        self.checks.append(description)
        print("PASS " + description, flush=True)

    def cli(self, device, *args, success=True):
        result = subprocess.run([self.binary, *args], cwd=self.root / device,
                                env=self.env(device), stdin=subprocess.DEVNULL,
                                capture_output=True, text=True, timeout=20)
        if (result.returncode == 0) != success:
            raise AssertionError(self.redacted(result.stderr))
        self.check(True, f"{device} {args[0] if args else 'connect'} exits as expected")
        return result

    def spawn(self, device, *args):
        out = self.root / f"process-{len(self.processes)}.stdout"
        err = self.root / f"process-{len(self.processes)}.stderr"
        out_file, err_file = out.open("wb"), err.open("wb")
        process = subprocess.Popen([self.binary, *args], cwd=self.root / device,
                                   env=self.env(device), stdin=subprocess.DEVNULL,
                                   stdout=out_file, stderr=err_file, start_new_session=True)
        self.processes.append(process)
        self.logs.append((out, err, out_file, err_file))
        return process

    def unsigned(self, name, *arguments):
        return self.spawn("B", "--server", self.state.origin,
                          "--allow", self.state.allow_principal, "--name", name,
                          "--no-setup", "--shell", "/bin/sh", "--reconnect-delay", "1",
                          *arguments)

    def connection(self, name):
        return wait_for(lambda: self.state.connections.get(name, [None])[-1],
                        f"{name} opens an outbound WebSocket")

    def stop(self, process, expected=0, sig=signal.SIGINT):
        owned = descendants(process.pid)
        if process.poll() is None:
            process.send_signal(sig)
        process.wait(timeout=8)
        if expected is not None:
            self.check(process.returncode == expected, "controlled session exits cleanly")
        wait_for(lambda: all(not process_running(pid) for pid in owned), "owned relay and shell cleanup")

    def cleanup(self):
        for process in reversed(self.processes):
            if process.poll() is None:
                try:
                    self.stop(process, expected=None, sig=signal.SIGTERM)
                except (subprocess.TimeoutExpired, AssertionError):
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=3)
        for _, _, out_file, err_file in self.logs:
            out_file.close()
            err_file.close()
        self.server.shutdown()
        self.server.server_close()
        self.foreign_server.shutdown()
        self.foreign_server.server_close()

    def diagnostics(self):
        print("Gateway protocol errors:", self.redacted(json.dumps(self.state.errors + self.foreign.errors)))
        for out, err, out_file, err_file in self.logs:
            out_file.flush()
            err_file.flush()
            print(f"\n{err.name}: {self.redacted(err.read_text(errors='replace')[-6000:])}")

    def run(self):
        retired = self.cli("A", "--ssh", "legacy.example", success=False)
        self.check("SSH device connections have been removed" in retired.stderr,
                   "legacy SSH stops before any transport starts")
        for command in ("invite", "join"):
            old = self.cli("A", command, "--help", success=False)
            self.check("invite and join were removed" in old.stderr,
                       f"retired {command} cannot enter a nonexistent invitation flow")
        self.check(not self.state.requests, "retired commands never contact the relay")
        login = self.cli("A", "login", "--server", self.state.origin, "--no-browser", "--json")
        self.check(json.loads(login.stdout)["identity"] == IDENTITY,
                   "device login stores the verified identity and registered server username")
        self.check(self.state.account_token not in login.stdout + login.stderr
                   and self.state.device_code not in login.stdout + login.stderr,
                   "login output excludes account and polling credentials")
        credential_file = self.root / "A/auth" / (hashlib.sha256(self.state.origin.encode()).hexdigest() + ".json")
        self.check(stat.S_IMODE(credential_file.stat().st_mode) == 0o600,
                   "account credentials have owner-only file permissions")
        whoami = self.cli("A", "whoami", "--json")
        self.check(json.loads(whoami.stdout)["identity"] == IDENTITY
                   and self.state.account_token not in whoami.stdout + whoami.stderr,
                   "whoami verifies identity without exposing the login token")
        original_credential = credential_file.read_bytes()
        before = len(self.state.requests)
        wrong_origin = self.cli("B", "--server", self.state.origin,
                                "--allow", self.foreign.allow_principal,
                                "--no-watchdog", "--no-setup", success=False)
        self.check("this connection uses" in wrong_origin.stderr and len(self.state.requests) == before
                   and not self.foreign.requests,
                   "an allow target from another relay is rejected before network access")
        old_allow = self.cli("B", "--server", self.state.origin, "--allow", "fixture@github",
                             "--no-watchdog", "--no-setup", success=False)
        self.check("no longer an allow target" in old_allow.stderr
                   and len(self.state.requests) == before,
                   "provider-style allow labels cannot replace a same-origin account target")
        self.cli("A", "whoami", "--server", self.foreign.origin, "--json", success=False)
        self.check(not self.foreign.requests and credential_file.read_bytes() == original_credential,
                   "selecting another origin never borrows the saved account credential")
        self.state.redirect_whoami = self.foreign.origin + "/api/client/whoami"
        redirected = self.cli("A", "whoami", "--json", success=False)
        self.state.redirect_whoami = None
        self.check("redirected" in redirected.stderr and not self.foreign.requests,
                   "account API redirects never forward credentials to another origin")
        self.state.whoami_identity = {**IDENTITY, "subject": "456"}
        changed = self.cli("A", "--no-watchdog", "--no-setup", "--shell", "/bin/sh", success=False)
        self.state.whoami_identity = None
        self.check("different GitHub account" in changed.stderr
                   and not self.state.connect_attempts
                   and credential_file.read_bytes() == original_credential,
                   "a changed stable identity cannot connect or replace the saved credential")
        mismatched = self.cli("A", f"other@{self.state.origin}", "--no-browser",
                              "--no-watchdog", "--no-setup", success=False)
        self.check("different username" in mismatched.stderr and not self.state.connect_attempts,
                   "a requested username must match the account registered on this relay")
        a = self.spawn("A", "--name", "A fixture", "--no-setup", "--shell", "/bin/sh",
                       "--reconnect-delay", "1")
        account_session = self.connection("A fixture")
        self.check(account_session.token == self.state.account_token
                   and account_session.hello["metadata"]["allow"] == [self.state.allow_principal],
                   "signed-in A binds its own username and origin with its saved account token")
        self.stop(a)

        b = self.unsigned("B fixture")
        connection = self.connection("B fixture")
        self.check(not connection.token
                   and connection.hello["version"] in SUPPORTED_WIRE_VERSIONS,
                   "unsigned B uses explicit same-origin allow and a supported WebSocket protocol")
        self.check(not list((self.root / "B/auth").iterdir()),
                   "unsigned B never saves or receives A's account credential")
        listing = json.loads(self.cli("A", "list", "--json").stdout)
        self.check(any(item["id"] == connection.id for item in listing["clients"]),
                   "A lists its currently controllable unsigned B session")
        terminal = connection.call("terminal.status", {})
        self.check(isinstance(terminal, dict), "B exposes a real terminal through the relay")
        job = connection.call("ssmcp.exec.start", {"command": [
            "/bin/sh", "-c", "test -z \"$SSMCP_RELAY_TOKEN\" && printf 'relay-shell-ok'"]})
        connection.call("ssmcp.exec.wait", {"job_id": job["job_id"], "wait_ms": 5000})
        output = connection.call("ssmcp.exec.read", {"job_id": job["job_id"], "max_bytes": 4096})
        self.check(output["exit_code"] == 0 and output["stdout"] == "relay-shell-ok",
                   "real shell round-trip succeeds without relay credentials reaching native tools")
        before = self.state.connect_attempts[""]
        connection.close(1001)
        wait_for(lambda: len(self.state.connections.get("B fixture", [])) >= 2,
                 "default watchdog reconnects after ordinary network close")
        reconnected = self.state.connections["B fixture"][-1]
        self.check(not reconnected.token and reconnected.id != connection.id
                   and self.state.connect_attempts[""] == before + 1,
                   "watchdog reconnects the unsigned device without acquiring a credential")
        reconnected.call("terminal.status", {})
        owned = descendants(b.pid)
        before = self.state.connect_attempts[""]
        reconnected.close(4001)
        b.wait(timeout=8)
        self.check(b.returncode != 0, "server access revocation terminates B with an error")
        wait_for(lambda: all(not process_running(pid) for pid in owned), "revocation process cleanup")
        self.check(self.state.connect_attempts[""] == before,
                   "revoked active sessions stop watchdog retries")

        self.state.next_revoke_on_ready = True
        before = self.state.connect_attempts[""]
        early = self.unsigned("B early revocation")
        early.wait(timeout=8)
        self.check(early.returncode != 0 and self.state.connect_attempts[""] == before + 1,
                   "revocation immediately after Ready also stops startup retries")
        for status in (401, 403):
            self.state.reject_connect = status
            before = self.state.connect_attempts[""]
            rejected = self.unsigned(f"B HTTP {status}")
            rejected.wait(timeout=8)
            self.state.reject_connect = None
            self.check(rejected.returncode != 0 and self.state.connect_attempts[""] == before + 1,
                       f"HTTP {status} policy failures stop default watchdog retries")
        stopped = self.unsigned("B interrupt")
        self.connection("B interrupt")
        self.stop(stopped)
        self.check(not list((self.root / "B/auth").iterdir()),
                   "Ctrl-C cleans up an unsigned session without leaving credentials on B")
        self.check(not any(path in ("/api/client/invites", "/api/client/enroll")
                           for _, path in self.state.requests),
                   "the current lifecycle never depends on invitation or enrollment endpoints")

        live = self.spawn("A", "--name", "A logout", "--no-setup", "--shell", "/bin/sh",
                          "--reconnect-delay", "1")
        self.connection("A logout")
        owned = descendants(live.pid)
        before = self.state.connect_attempts[self.state.account_token]
        authorizations = self.state.requests.count(("POST", "/api/client/login/start"))
        self.cli("A", "logout", "--json")
        live.wait(timeout=8)
        wait_for(lambda: all(not process_running(pid) for pid in owned), "logout process cleanup")
        after = self.state.connect_attempts[self.state.account_token]
        self.check(live.returncode != 0
                   and after == before,
                   "logout revokes its live session and declined authorization cannot reconnect it"
                   f" (exit={live.returncode}, connect_attempts_before={before}, connect_attempts_after={after})")
        self.check(self.state.requests.count(("POST", "/api/client/login/start")) == authorizations + 1,
                   "a foreground account session offers at most one fresh authorization after revocation")
        self.check(not credential_file.exists() and self.state.logged_out,
                   "logout revokes and removes the selected origin's account credential")
        self.check(not self.state.errors and not self.foreign.errors,
                   "gateways observed no malformed client protocol frames")
        return {"status": "passed", "checks": len(self.checks), "passed_checks": self.checks, "transport": "loopback WebSocket",
                "oauth": "synthetic GitHub provider", "device": "unsigned explicit same-origin allow",
                "ssh": False, "models_started": False,
                "binary_sha256": hashlib.file_digest(Path(self.binary).open("rb"), "sha256").hexdigest(),
                "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/ssmcp"))
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error("build ssmcp first or provide --binary")
    with tempfile.TemporaryDirectory(prefix="ssmcp-account-relay-") as temporary:
        harness = Harness(Path(temporary), binary)
        try:
            result = harness.run()
            if args.report:
                args.report.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
            print(json.dumps(result, ensure_ascii=False))
        except Exception as error:
            harness.diagnostics()
            raise SystemExit(harness.redacted(f"{type(error).__name__}: {error}")) from None
        finally:
            harness.cleanup()

if __name__ == "__main__":
    main()
