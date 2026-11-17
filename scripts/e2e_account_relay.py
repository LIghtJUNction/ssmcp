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
import shlex
import signal
import socket
import stat
import struct
import subprocess
import tempfile
import threading
import time

IDENTITY = {"provider": "github", "subject": "123", "login": "fixture", "principal": "fixture@github"}
MAX_FRAME = 4 * 1024 * 1024

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
            assert self.hello["version"] == 5
            assert self.hello["mode"] == "shell"
            assert self.hello["metadata"]["allow"] == [IDENTITY["principal"]]
            self.send({"type": "ready", "url": self.state.origin + "/mcp",
                       "client_id": self.id, "login_code": "", "login_expires_in": 0})
            with self.state.lock:
                self.state.sessions[self.id] = self
                self.state.connections.setdefault(self.token, []).append(self)
            if self.state.devices.get(self.token, {}).get("revoke_on_ready"):
                self.close(4001)
            while True:
                frame = self.read()
                assert frame["type"] == "reply"
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
        self.invites = {}
        self.issued_invites = []
        self.devices = {}
        self.next_revoke_on_ready = False
        self.enroll_count = 0
        self.requests = []
        self.connect_attempts = {}
        self.next_reject = None
        self.rejected = {}
        self.errors = []
        self.origin = ""
        self.logged_out = False

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
    def respond(self, status, value):
        encoded = json.dumps(value).encode()
        self.send_response(status)
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
            if token in self.state.rejected:
                return self.respond(self.state.rejected[token], {"error": "invalid_token"})
            if not self.state.identity(token) and token not in self.state.devices:
                return self.respond(401, {"error": "invalid_token"})
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
            return self.respond(200, {"identity": IDENTITY})
        if self.path == "/api/client/sessions":
            with self.state.lock:
                clients = [{"id": connection.id, "type": "shell", **connection.hello["metadata"]}
                           for connection in self.state.sessions.values()]
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
            if self.state.pending_polls:
                self.state.pending_polls -= 1
                return self.respond(200, {"status": "pending"})
            return self.respond(200, {"status": "authorized", "access_token": self.state.account_token,
                                      "identity": IDENTITY, "expires_in": 3600})
        if self.path == "/api/client/enroll":
            with self.state.lock:
                invite = self.state.invites.pop(body.get("token", ""), None)
                if invite is None:
                    return self.respond(409, {"error": "invalid_token"})
                token = "cd_" + secrets.token_hex(32)
                self.state.devices[token] = dict(invite, revoke_on_ready=self.state.next_revoke_on_ready)
                self.state.next_revoke_on_ready = False
                self.state.enroll_count += 1
                if self.state.next_reject is not None:
                    self.state.rejected[token] = self.state.next_reject
                    self.state.next_reject = None
            return self.respond(200, {"device_token": token, "principal": IDENTITY["principal"],
                                      "expires_in": invite["session_ttl_secs"]})
        if not self.state.identity(self.bearer()):
            return self.respond(401, {"error": "invalid_token"})
        if self.path == "/api/client/invites":
            token = "ci_" + secrets.token_hex(32)
            self.state.invites[token] = body
            self.state.issued_invites.append(token)
            return self.respond(200, {"token": token, "expires_in": body["ttl_secs"],
                                      "session_expires_in": body["session_ttl_secs"]})
        if self.path == "/api/client/logout":
            self.state.logged_out = True
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
                      *self.state.devices, *self.state.issued_invites]:
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

    def invite(self):
        output = self.cli("A", "invite", "--no-bootstrap", "--json")
        args = shlex.split(json.loads(output.stdout)["command"])
        self.check(args[:2] == ["ssmcp", "join"], "invite produces a validated B join command")
        return args[1:]

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

    def diagnostics(self):
        for out, err, out_file, err_file in self.logs:
            out_file.flush()
            err_file.flush()
            print(f"\n{err.name}: {self.redacted(err.read_text(errors='replace')[-6000:])}")

    def run(self):
        login = self.cli("A", "login", "--server", self.state.origin, "--no-browser", "--json")
        self.check(json.loads(login.stdout)["identity"] == IDENTITY,
                   "device login stores the verified synthetic GitHub identity")
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
        a = self.spawn("A", "--name", "A fixture", "--no-setup", "--shell", "/bin/sh",
                       "--reconnect-delay", "1")
        wait_for(lambda: self.state.connections.get(self.state.account_token),
                 "logged-in A connects without --allow")
        account_session = self.state.connections[self.state.account_token][-1]
        self.check(account_session.hello["metadata"]["allow"] == [IDENTITY["principal"]],
                   "logged-in client restricts access to its GitHub account automatically")
        self.stop(a)
        command = self.invite()
        b = self.spawn("B", *command, "--name", "B fixture", "--no-setup",
                       "--shell", "/bin/sh", "--reconnect-delay", "1")
        token = wait_for(lambda: next(iter(self.state.devices), None), "B consumes invitation")
        wait_for(lambda: self.state.connections.get(token), "B opens outbound WebSocket")
        connection = self.state.connections[token][-1]
        self.check(self.state.enroll_count == 1 and connection.hello["version"] == 5,
                   "B joins with one enrollment and a version 5 WebSocket handshake")
        self.check(not list((self.root / "B/auth").iterdir()),
                   "temporary B session never saves A's account credentials")
        listing = json.loads(self.cli("A", "list", "--json").stdout)
        self.check(any(item["id"] == connection.id for item in listing["clients"]),
                   "A lists its currently controllable B session")
        terminal = connection.call("terminal.status", {})
        self.check(isinstance(terminal, dict), "B exposes a real terminal through the relay")
        job = connection.call("ssmcp.exec.start", {"command": [
            "/bin/sh", "-c", "test -z \"$SSMCP_RELAY_TOKEN\" && printf 'relay-shell-ok'"]})
        connection.call("ssmcp.exec.wait", {"job_id": job["job_id"], "wait_ms": 5000})
        output = connection.call("ssmcp.exec.read", {"job_id": job["job_id"], "max_bytes": 4096})
        self.check(output["exit_code"] == 0 and output["stdout"] == "relay-shell-ok",
                   "real shell round-trip succeeds without relay credentials reaching native tools")
        self.cli("B", *command, "--no-watchdog", success=False)
        self.check(self.state.enroll_count == 1, "consumed invitations cannot enroll B again")
        connection.close(1001)
        wait_for(lambda: len(self.state.connections.get(token, [])) >= 2,
                 "default watchdog reconnects after ordinary network close")
        reconnected = self.state.connections[token][-1]
        self.check(self.state.enroll_count == 1 and reconnected.id != connection.id,
                   "watchdog reuses the in-memory device credential, never the invitation")
        reconnected.call("terminal.status", {})
        owned = descendants(b.pid)
        reconnected.close(4001)
        b.wait(timeout=8)
        self.check(b.returncode != 0, "server revocation terminates B with an error")
        wait_for(lambda: all(not process_running(pid) for pid in owned), "revocation process cleanup")
        self.check(self.state.connect_attempts[token] == 2,
                   "revoked active sessions stop watchdog retries")
        self.state.next_revoke_on_ready = True
        early_command = self.invite()
        known_tokens = set(self.state.devices)
        early = self.spawn("B", *early_command, "--no-setup", "--shell", "/bin/sh",
                           "--reconnect-delay", "1")
        early.wait(timeout=8)
        early_token = (set(self.state.devices) - known_tokens).pop()
        self.check(early.returncode != 0 and self.state.connect_attempts[early_token] == 1,
                   "revocation immediately after Ready also stops startup retries")
        for status in (401, 403):
            self.state.next_reject = status
            next_command = self.invite()
            known_tokens = set(self.state.devices)
            rejected = self.spawn("B", *next_command, "--no-setup", "--shell", "/bin/sh",
                                  "--reconnect-delay", "1")
            rejected.wait(timeout=8)
            fresh = (set(self.state.devices) - known_tokens).pop()
            self.check(rejected.returncode != 0 and self.state.connect_attempts[fresh] == 1,
                       f"HTTP {status} authentication failures stop default watchdog retries")
        next_command = self.invite()
        known_tokens = set(self.state.devices)
        stopped = self.spawn("B", *next_command, "--no-setup", "--shell", "/bin/sh")
        fresh = wait_for(lambda: next(iter(set(self.state.devices) - known_tokens), None), "fresh B token")
        wait_for(lambda: self.state.connections.get(fresh), "fresh B session")
        self.stop(stopped)
        self.check(not list((self.root / "B/auth").iterdir()),
                   "Ctrl-C cleans up a temporary session without leaving credentials on B")
        self.check(not self.state.errors, "gateway observed no malformed client protocol frames")
        self.cli("A", "logout", "--json")
        self.check(not credential_file.exists(), "logout revokes and removes the account credential")
        return {"status": "passed", "checks": len(self.checks), "transport": "loopback WebSocket",
                "oauth": "synthetic GitHub provider", "ssh": False, "models_started": False}

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
