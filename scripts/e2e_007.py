#!/usr/bin/env python3
"""Test 007 against a private loopback relay and synthetic credentials.

Python standard library only. The default fixture uses an independent temporary
session manager so no installed unit or real account is changed. Native systemd
unit/cgroup validation must be recorded separately on the target Linux machine.

Add --fixture-binary PATH to also call the recovery client through the production
Broker and HTTP router in an already-built library test executable. For the
bounded protocol regression alone, also pass --routing-only. This reuses the
relay composer fixture's synthetic GitHub provider and loopback HTTP/WS; it does
not validate TLS, a real GitHub account, or an installed gateway daemon.
"""
from __future__ import annotations

import argparse
import base64
import hashlib
from http.server import ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
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

import e2e_account_relay as relay

TOOLS = {
    "ssmcp.007.status", "ssmcp.007.list", "ssmcp.007.wake",
    "ssmcp.007.close", "ssmcp.007.remove",
}


class NativeFailure(RuntimeError):
    pass


def worker_identity(status):
    connection = status.get("connection") or {}
    pid, client_id = status.get("manager_pid"), connection.get("client_id")
    if not isinstance(pid, int) or pid <= 0 or not client_id:
        raise AssertionError("connected worker lacks a live process or broker identity")
    return pid, client_id


def unwrap(reply):
    if "Err" in reply:
        raise NativeFailure(reply["Err"])
    value = reply["Ok"]
    if isinstance(value, dict) and value.get("isError"):
        raise NativeFailure(json.dumps(value))
    return value.get("structuredContent", value) if isinstance(value, dict) else value


class Socket(relay.WebSocket):
    def __init__(self, *args):
        super().__init__(*args)
        self.client_pings = 0
        self.client_pongs = 0
        self.silent = False
        self.accepted_at = time.monotonic()

    def read(self):
        while True:
            first, second = self.exact(2)
            assert first & 0x80, "fixture expects complete frames"
            opcode = first & 15
            assert second & 0x80, "client WebSocket frames must be masked"
            size = second & 127
            if size == 126:
                size = struct.unpack("!H", self.exact(2))[0]
            elif size == 127:
                size = struct.unpack("!Q", self.exact(8))[0]
            assert size <= relay.MAX_FRAME, "oversized client frame"
            mask = self.exact(4)
            payload = bytes(value ^ mask[index % 4] for index, value in enumerate(self.exact(size)))
            if opcode == 8:
                raise EOFError()
            if opcode == 9:
                self.client_pings += 1
                self.send(payload, 10)
                continue
            if opcode == 10:
                self.client_pongs += 1
                continue
            assert opcode in (1, 2), "unexpected WebSocket opcode"
            return json.loads(payload)


    def serve(self):
        try:
            self.hello = self.read()
            assert self.hello["type"] == "hello"
            if self.hello["metadata"]["name"] == self.state.recovery_name:
                assert self.hello["version"] == 7, "007 must advertise the current v7 protocol"
            else:
                assert self.hello["version"] in relay.SUPPORTED_WIRE_VERSIONS
            assert self.hello["mode"] == "shell"
            assert self.hello["metadata"].get("allow", []) in ([], [self.state.allow_principal])
            ready = {"type": "ready", "url": self.state.origin + "/mcp",
                     "client_id": self.id, "login_code": "", "login_expires_in": 0}
            if self.hello["metadata"]["name"] != self.state.recovery_name:
                ready["capabilities"] = list(self.state.capabilities)
            self.send(ready)
            with self.state.lock:
                self.state.sessions[self.id] = self
                self.state.connections.setdefault(self.token, []).append(self)
            while True:
                frame = self.read()
                if frame["type"] == "access":
                    # Full clients read the relay-owned allow list at startup.
                    assert self.hello["metadata"]["name"] != self.state.recovery_name
                    assert frame["command"] == {"action": "list"}
                    self.send({"type": "access_reply", "id": frame["id"], "result": {"Ok": {
                        "entries": [{"principal": self.state.allow_principal,
                                     "login": relay.IDENTITY["username"],
                                     "subject": relay.IDENTITY["subject"],
                                     "owner": bool(self.token)}],
                    }}})
                    continue
                if frame["type"] == "steering":
                    # Woken full clients use v7 presence polling. This recovery
                    # fixture has no queue: do not simulate publication or delivery.
                    assert self.hello["version"] == 7
                    assert self.hello["metadata"]["name"] != self.state.recovery_name
                    assert frame["command"] == {"op": "snapshot"}
                    self.send({"type": "steering_reply", "id": frame["id"], "result": {"Ok": {
                        "schema_version": 1, "client_id": self.id, "revision": 0,
                        "agents": [], "messages": [],
                    }}})
                    continue
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

    def operation(self, operation, timeout=45):
        with self.state.lock:
            self.sequence += 1
            identifier = self.sequence
            result = queue.Queue(maxsize=1)
            self.pending[identifier] = result
        try:
            self.send({"type": "call", "id": identifier, "operation": operation})
            return unwrap(result.get(timeout=timeout))
        finally:
            with self.state.lock:
                self.pending.pop(identifier, None)

    def tool(self, name, arguments):
        return self.operation({"op": "call_tool", "name": name, "arguments": arguments})


class Handler(relay.Handler):
    def do_GET(self):
        if self.path != "/api/client/connect":
            return super().do_GET()
        self.state.requests.append(("GET", self.path))
        token = self.bearer()
        self.state.connect_attempts[token] = self.state.connect_attempts.get(token, 0) + 1
        if token in self.state.rejected:
            return self.respond(self.state.rejected[token], {"error": "invalid_token"})
        if not self.state.identity(token):
            return self.respond(401, {"error": "invalid_token"})
        key = self.headers["Sec-WebSocket-Key"]
        accept = base64.b64encode(hashlib.sha1(
            (key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()
        ).digest()).decode()
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        self.close_connection = True
        self.connection.settimeout(110)
        Socket(self, token, self.state).serve()


class Fixture:
    def __init__(self, root, binary, native_systemd=False):
        self.root = root
        self.native_systemd = native_systemd
        self.binary = binary
        self.workspace = root / "workspace"
        self.workspace.mkdir(mode=0o700)
        (self.workspace / "child").mkdir()
        self.home = root / "007"
        self.home.mkdir(mode=0o700)
        self.auth = root / "auth"
        self.auth.mkdir(mode=0o700)
        self.sessions = self.home / "sessions"
        self.sessions.mkdir(mode=0o700)
        self.snapshot = self.home / "ssmcp"
        self.installed = root / "installed-ssmcp"
        shutil.copy2(binary, self.snapshot)
        shutil.copy2(binary, self.installed)
        self.state = relay.State()
        self.state.rejected = {}
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.server.state = self.state
        self.state.origin = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        self.config = {
            "format": 1, "server": self.state.origin, "name": "007-fixture",
            "cwd": str(self.workspace), "client": str(self.installed),
        }
        self.state.recovery_name = self.config["name"]
        self.private_json(self.home / "config.json", self.config)
        self.write_credential()
        self.environment = {
            key: value for key, value in os.environ.items()
            if not key.startswith("SSMCP_")
        }
        minimal = root / "minimal-path"
        minimal.mkdir()
        for command in ("sh", "sleep", "systemctl", "systemd-run"):
            source = shutil.which(command)
            if source:
                (minimal / command).symlink_to(source)
        self.environment.update(
            PATH=str(minimal), SSMCP_007_HOME=str(self.home),
            SSMCP_SESSION_HOME=str(self.sessions),
            SSMCP_AUTH_HOME=str(self.auth),
            XDG_CONFIG_HOME=str(root / "config"), NO_COLOR="1", TERM="dumb",
        )
        self.processes = []
        self.logfiles = []
        self.checks = []
        self.names = set()
        self.zero = None
        self.manager = None
        self.stopping = threading.Event()
        self.heartbeat = threading.Thread(target=self.ping_loop, daemon=True)
        self.heartbeat.start()

    @staticmethod
    def private_json(path, value):
        descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(descriptor, "w") as handle:
            json.dump(value, handle)

    def write_credential(self):
        name = hashlib.sha256(self.state.origin.encode()).hexdigest() + ".json"
        self.private_json(self.auth / name, {
            "server": self.state.origin, "token": self.state.account_token,
            "principal": relay.IDENTITY["principal"], "subject": relay.IDENTITY["subject"],
            "expires_at": int(time.time()) + 3600,
        })

    def ping_loop(self):
        while not self.stopping.wait(1):
            with self.state.lock:
                sockets = list(self.state.sessions.values())
            for connection in sockets:
                if not connection.silent and not connection.closed.is_set():
                    try:
                        connection.send(b"", 9)
                    except OSError:
                        pass

    def check(self, condition, description):
        if not condition:
            raise AssertionError(description)
        self.checks.append(description)
        print("PASS " + description, flush=True)

    def spawn(self, label, *args):
        logfile = (self.root / (label + ".log")).open("wb")
        self.logfiles.append(logfile)
        process = subprocess.Popen(
            [str(self.snapshot), *args], cwd=self.workspace, env=self.environment,
            stdin=subprocess.DEVNULL, stdout=logfile, stderr=logfile,
            start_new_session=True,
        )
        self.processes.append(process)
        return process

    def cli(self, *args, success=True):
        result = subprocess.run(
            [str(self.snapshot), *args], cwd=self.workspace, env=self.environment,
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=55,
        )
        if success and result.returncode:
            raise AssertionError(f"fixture CLI failed: {result.stderr[-1500:]}")
        return result

    def start(self):
        if self.native_systemd:
            result = subprocess.run(
                ["systemctl", "--user", "show", "--property=Version"],
                capture_output=True, text=True, timeout=5, check=False,
            )
            if result.returncode:
                raise RuntimeError("native test requires a working systemd user manager")
        else:
            self.manager = self.spawn("manager", "service", "run")
            relay.wait_for(lambda: (self.sessions / "service.json").is_file(),
                           "private session manager", timeout=10)
        return self.start_zero()

    def start_zero(self):
        self.zero = self.spawn("007-" + str(len(self.processes)), "007", "run")
        return self.connected_zero()

    def connected_zero(self, previous=None, timeout=35):
        def find():
            if self.zero.poll() is not None:
                raise AssertionError("007 process exited before connecting")
            with self.state.lock:
                return next((
                    connection for connection in self.state.sessions.values()
                    if connection.hello["metadata"]["name"] == self.config["name"]
                    and connection.id != previous
                ), None)
        return relay.wait_for(find, "007 registered with fixture relay", timeout=timeout)

    def session_status(self, name):
        return json.loads(self.cli("sessions", "status", name, "--json").stdout)

    def worker_connected(self, name, timeout=35):
        def observe():
            value = self.session_status(name)
            return value if value.get("connection_state") == "connected" else None
        return relay.wait_for(observe, "formal session registered", timeout=timeout)

    def native_failure(self, connection, name, arguments, description):
        try:
            connection.tool(name, arguments)
        except NativeFailure:
            self.check(True, description)
            return
        raise AssertionError(description)

    def run(self, idle_timeout=False, routing_only=False):
        connection = self.start_zero() if routing_only else self.start()
        self.check(connection.hello["version"] == 7,
                   "the actual recovery Hello advertises v7 with only recovery capabilities")
        self.check(relay.descendants(self.zero.pid) == [], "idle 007 starts no relay child, PTY or tool backend")
        listed = connection.operation({"op": "list_tools", "cursor": None})
        self.check({item["name"] for item in listed["tools"]} == TOOLS,
                   "007 exposes exactly five narrow recovery tools")
        status = connection.tool("ssmcp.007.status", {})
        self.check(isinstance(status, dict), "007 status returns an object")
        if routing_only:
            self.check(not self.state.errors, "the bounded recovery protocol fixture observed no errors")
            return
        self.native_failure(connection, "terminal.run", {"command": "true"},
                            "007 rejects general command execution")
        self.native_failure(connection, "ssmcp.007.wake", {"name": "bad", "args": ["sh"]},
                            "wake rejects arbitrary startup arguments")
        self.native_failure(connection, "ssmcp.007.wake", {"name": "../escape"},
                            "wake rejects traversal in session names")
        self.native_failure(connection, "ssmcp.007.wake", {"name": "escape", "cwd": ".."},
                            "wake rejects workspace traversal")
        (self.workspace / "escape").symlink_to(self.root)
        self.native_failure(connection, "ssmcp.007.wake", {"name": "escape", "cwd": "escape"},
                            "wake rejects an escaping cwd symlink")
        relay.wait_for(lambda: connection.client_pongs >= 2, "007 responds to relay keepalive", timeout=5)
        self.check(connection.client_pings == 0, "007 relies on relay pings without a second heartbeat source")

        first_id = connection.id
        connection.close()
        connection = self.connected_zero(first_id)
        self.check(connection.id != first_id, "closed relay transport reconnects automatically")

        cache = self.home / "state.json"
        saved_cache = self.home / "state.saved"
        if cache.exists():
            cache.replace(saved_cache)
        cache.mkdir()
        try:
            old_id = connection.id
            connection.close()
            connection = self.connected_zero(old_id)
            self.check(isinstance(connection.tool("ssmcp.007.status", {}), dict),
                       "an unwritable observation cache does not prevent reconnection or status RPC")
        finally:
            cache.rmdir()
            if saved_cache.exists():
                saved_cache.replace(cache)

        old_id = connection.id
        try:
            connection.send(b"x" * (128 * 1024))
        except OSError:
            pass
        connection = self.connected_zero(old_id)
        self.check(connection.id != old_id, "oversized frame is rejected and the recovery loop reconnects")

        created = connection.tool("ssmcp.007.wake", {"name": "worker", "cwd": "child"})
        worker_name = created.get("name", "worker")
        self.names.add(worker_name)
        worker = self.worker_connected(worker_name)
        self.check(Path(worker["cwd"]) == self.workspace / "child",
                   "wake starts a real formal session in the approved workspace")
        again = connection.tool("ssmcp.007.wake", {"name": "worker", "cwd": "child"})
        after = self.session_status(worker_name)
        self.check(worker_identity(after) == worker_identity(worker),
                   "repeated wake preserves the worker process and broker connection")
        if self.native_systemd:
            control_group = Path(f"/proc/{self.zero.pid}/cgroup").read_text()
            worker_group = Path(f"/proc/{worker_identity(worker)[0]}/cgroup").read_text()
            self.check(control_group != worker_group and ".service" in worker_group,
                       "the native worker has an independent systemd service cgroup")
        self.check(again.get("name", worker_name) == worker_name, "wake retry keeps the saved session identity")

        with self.state.lock:
            worker_socket = next(item for item in self.state.sessions.values()
                                 if item.hello["metadata"]["name"] == worker_name)
            workers_before = {worker_socket.id}
        marker = self.workspace / "child" / "ongoing-job"
        job = worker_socket.tool("ssmcp.exec.start", {
            "command": [sys.executable, "-c",
                        "import time; f=open('ongoing-job','ab',buffering=0); "
                        "exec('while True:\\n f.write(b\".\"); time.sleep(.1)')"],
        })
        relay.wait_for(lambda: marker.exists() and marker.stat().st_size > 0,
                       "long-running formal-session job", timeout=10)
        marker_before = marker.stat().st_size
        self.zero.send_signal(signal.SIGTERM)
        self.zero.wait(timeout=10)
        preserved = self.worker_connected(worker_name)
        self.check(worker_identity(preserved) == worker_identity(worker),
                   "stopping 007 preserves the independent formal session")
        connection = self.start_zero()
        self.check(worker_identity(self.session_status(worker_name)) == worker_identity(worker),
                   "restarting 007 preserves the same formal-session process and connection")
        with self.state.lock:
            live_ids = set(self.state.sessions)
        self.check(workers_before.issubset(live_ids),
                   "formal-session relay transport stays connected across 007 restart")
        relay.wait_for(lambda: marker.stat().st_size > marker_before,
                       "formal-session job keeps making progress", timeout=5)
        job_state = worker_socket.tool("ssmcp.exec.status", {"job_id": job["job_id"]})
        self.check(job_state["running"], "an in-flight formal-session job survives 007 stop and restart")

        checksum = hashlib.sha256(self.snapshot.read_bytes()).hexdigest()
        bad = self.root / "broken-client"
        bad.write_text("#!/bin/sh\nexit 86\n")
        bad.chmod(0o700)
        bad.replace(self.installed)
        self.native_failure(connection, "ssmcp.007.wake", {"name": "upgrade-broken"},
                            "a failed installed client reports failure without stopping 007")
        self.check(isinstance(connection.tool("ssmcp.007.status", {}), dict),
                   "007 remains controllable after a broken normal-client upgrade")
        recovered = connection.tool("ssmcp.007.wake", {"name": "recovery", "recovery": True})
        recovery_name = recovered.get("name", "recovery")
        self.names.add(recovery_name)
        recovery_status = self.worker_connected(recovery_name)
        self.check(hashlib.sha256(self.snapshot.read_bytes()).hexdigest() == checksum,
                   "the fixed recovery image survives installed-client replacement")
        self.check(True, "recovery=true starts a usable formal session from the fixed image")

        connection.tool("ssmcp.007.close", {"name": worker_name})
        closed = self.session_status(worker_name)
        self.check(not closed["manager_active"] and closed["stop_requested"],
                   "close persists stopped intent and confirms the worker stopped")
        connection.tool("ssmcp.007.wake", {"name": worker_name, "cwd": "child"})
        self.worker_connected(worker_name)
        self.check(True, "wake restarts an existing saved session while the installed client is broken")

        if idle_timeout:
            old_id = connection.id
            connection.silent = True
            started = time.monotonic()
            connection = self.connected_zero(old_id, timeout=100)
            elapsed = time.monotonic() - started
            self.check(65 <= elapsed <= 100,
                       "a silent live transport expires and reconnects within the receive deadline")

        old_token = self.state.account_token
        self.state.rejected[old_token] = 401
        old_id = connection.id
        old_attempts = self.state.connect_attempts.get(old_token, 0)
        connection.close()
        time.sleep(5)
        attempts = self.state.connect_attempts.get(old_token, 0) - old_attempts
        self.check(self.zero.poll() is None and 0 < attempts <= 5,
                   "authorization refusal keeps 007 alive with bounded retries")
        self.state.account_token = "cl_" + secrets.token_hex(32)
        self.write_credential()
        connection = self.connected_zero(old_id, timeout=75)
        self.check(True, "reconnection reloads repaired credentials from the private account store")

        removed = connection.tool("ssmcp.007.remove", {"name": recovery_name})
        self.check(removed.get("removed") is True,
                   "remove explicitly stops and deletes a running recovery session")
        relay.wait_for(lambda: not relay.process_running(worker_identity(recovery_status)[0]),
                       "removed recovery worker exits", timeout=10)
        self.names.discard(recovery_name)
        for name in sorted(self.names):
            connection.tool("ssmcp.007.close", {"name": name})
            connection.tool("ssmcp.007.remove", {"name": name})
        self.names.clear()
        listing = connection.tool("ssmcp.007.list", {})
        self.check(listing.get("sessions", []) == [], "remove clears only the private recovery-session inventory")
        self.check(relay.descendants(self.zero.pid) == [], "007 returns to child-free idle after session management")
        self.check(not self.state.errors, "fixture observed no protocol errors")

    def cleanup(self):
        failures = []
        if self.native_systemd or self.manager is not None and self.manager.poll() is None:
            for name in sorted(self.names):
                try:
                    self.cli("sessions", "remove", name, "--json")
                except Exception as error:
                    failures.append(str(error))
        self.stopping.set()
        for process in reversed(self.processes):
            if process.poll() is None:
                process.send_signal(signal.SIGTERM)
                try:
                    process.wait(timeout=35)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=5)
                    failures.append("a fixture process required forced cleanup")
        with self.state.lock:
            sockets = list(self.state.sessions.values())
        for connection in sockets:
            connection.close()
        self.server.shutdown()
        self.server.server_close()
        for handle in self.logfiles:
            handle.close()
        return failures


def gateway_regression(binary, fixture_binary):
    """Use real dispatch/completion ordering, not a mock Call/Reply exchange."""
    from e2e_relay_composer import RelayComposer, direct_loopback, private_json

    result = {
        "requested": True, "success": False,
        "fixture_binary": str(fixture_binary),
        "fixture_binary_sha256": hashlib.sha256(fixture_binary.read_bytes()).hexdigest(),
        "scope": "production Broker and HTTP router in a library test executable; synthetic GitHub; loopback HTTP/WS",
        "tls_tested": False, "real_credentials_used": False,
    }
    checks = []
    with tempfile.TemporaryDirectory(prefix="ssmcp-007-router-e2e-") as directory:
        root = Path(directory)
        # Only the router/OAuth harness is reused. No frontend is created and
        # this unused decoder path need not exist; 007 never creates a PTY.
        harness = RelayComposer(root, binary.parent, binary.parent / "examples/vt_snapshot", fixture_binary)

        def redacted(error):
            message = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    message = message.replace(secret, "[fixture-secret]")
            return message.replace(str(root), "<fixture>")

        try:
            harness.prepare()
            harness.authenticate_mcp()
            workspace, home, auth = root / "workspace", root / "007", root / "auth"
            for path in (workspace, home, auth, home / "sessions", root / "minimal-path"):
                path.mkdir(mode=0o700)
            shutil.copy2(binary, home / "ssmcp")
            name = "007-real-router"
            private_json(home / "config.json", {
                "format": 1, "server": harness.origin, "name": name,
                "cwd": str(workspace), "client": str(binary),
            })
            private_json(auth / (hashlib.sha256(harness.origin.encode()).hexdigest() + ".json"), {
                "server": harness.origin, "token": harness.account_token,
                "principal": harness.identity["principal"], "subject": harness.identity["subject"],
                "username": harness.identity["username"], "expires_at": harness.account_expires_at,
            })
            environment = direct_loopback({key: value for key, value in os.environ.items()
                                           if not key.startswith("SSMCP_")})
            environment.update(
                PATH=str(root / "minimal-path"), ENV="/dev/null", BASH_ENV="/dev/null",
                SSMCP_007_HOME=str(home), SSMCP_AUTH_HOME=str(auth),
                SSMCP_SESSION_HOME=str(home / "sessions"), XDG_CONFIG_HOME=str(root / "config"),
                NO_COLOR="1", TERM="dumb",
            )
            process = harness.spawn("007-router-client", [str(binary), "007", "run"],
                                    cwd=workspace, env=environment)

            def observation():
                if process.poll() is not None:
                    raise AssertionError("007 exited during the real-router regression")
                path = home / "state.json"
                return json.loads(path.read_text()) if path.is_file() else {}

            def connected():
                value = observation()
                return value if value.get("state") == "connected" and value.get("client_id") else None

            state = relay.wait_for(connected, "007 connected to the real router", timeout=12)
            harness.client_id = state["client_id"]
            harness.check(state.get("pid") == process.pid,
                          "real-router 007 reports the owned process and one live connection")
            clients = harness.account("/api/client/sessions")["clients"]
            harness.check(any(item["id"] == harness.client_id and item["name"] == name for item in clients),
                          "the actual account endpoint discovers the recovery connection")
            first = harness.call("ssmcp.007.status", {})
            harness.check(first.get("mode") == "007" and first.get("pid") == process.pid,
                          "the first real HTTP code_mode call reaches 007 and returns its native status")
            # A v7-advertising recovery client closes on CodeModeFinished here.
            # A second real request to the ORIGINAL ID must still succeed; a
            # reconnect with a different broker identity does not satisfy this.
            second = harness.call("ssmcp.007.list", {})
            harness.check(second.get("sessions") == [],
                          "a second real code_mode request reaches the original 007 connection after completion")
            final = observation()
            clients = harness.account("/api/client/sessions")["clients"]
            harness.check(final.get("state") == "connected" and final.get("client_id") == harness.client_id
                          and final.get("pid") == process.pid
                          and any(item["id"] == harness.client_id for item in clients),
                          "completed code_mode calls preserve the same recovery process and broker client ID")
            harness.check(relay.descendants(process.pid) == [],
                          "real gateway dispatch leaves recovery without a PTY, backend, or child process")
            result["success"] = True
        except Exception as error:
            result["error"] = redacted(error)
            print(result["error"], file=sys.stderr)
        finally:
            try:
                harness.cleanup()
                result["fixture_exit_code"] = harness.fixture_exit_code
                if harness.fixture_exit_code != 0:
                    result["success"] = False
                    result["cleanup_error"] = "the owned router fixture did not exit cleanly"
            except Exception as error:
                result["success"] = False
                result["cleanup_error"] = redacted(error)
            checks = list(harness.checks)
    result["check_count"] = len(checks)
    return result, checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--native-systemd", action="store_true",
                        help="create isolated real systemd user session units")
    parser.add_argument("--idle-timeout", action="store_true",
                        help="also wait for the 75-second silent-peer deadline")
    parser.add_argument("--fixture-binary", type=Path,
                        help="already-built library test executable containing the real relay router fixture")
    parser.add_argument("--routing-only", action="store_true",
                        help="check actual Hello and real code_mode connection retention; requires --fixture-binary")
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("007 integration requires Linux")
    if args.routing_only and (not args.fixture_binary or args.native_systemd or args.idle_timeout):
        parser.error("--routing-only requires --fixture-binary and excludes --native-systemd/--idle-timeout")
    binary = args.binary.resolve(strict=True)
    fixture_binary = args.fixture_binary.resolve(strict=True) if args.fixture_binary else None
    report = {
        "schema": "ssmcp.007.e2e.v1", "binary": str(binary),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "fixture": ("loopback synthetic relay Hello and real-router code_mode connection regression"
                    if args.routing_only else
                    "loopback synthetic account relay and isolated systemd user units"
                    if args.native_systemd else
                    "loopback synthetic account relay and independent private session manager"),
        "native_systemd_units_tested": args.native_systemd, "real_credentials_used": False,
        "routing_only": args.routing_only, "real_router": {"requested": fixture_binary is not None},
        "checks": [], "success": False,
    }
    with tempfile.TemporaryDirectory(prefix="ssmcp-007-e2e-") as directory:
        fixture = Fixture(Path(directory), binary, args.native_systemd)
        try:
            fixture.run(args.idle_timeout, args.routing_only)
            report["success"] = True
        except Exception as error:
            report["error"] = f"{type(error).__name__}: {error}"
            print(report["error"], file=sys.stderr)
            for path in sorted(Path(directory).glob("*.log")):
                report.setdefault("logs", {})[path.name] = path.read_text(
                    errors="replace"
                )[-6000:].replace(fixture.state.account_token, "[fixture-token]")
        finally:
            report["checks"] = fixture.checks
            report["cleanup_errors"] = fixture.cleanup()
            if report["cleanup_errors"]:
                report["success"] = False
    if report["success"] and fixture_binary is not None:
        report["real_router"], gateway_checks = gateway_regression(binary, fixture_binary)
        report["checks"].extend(gateway_checks)
        report["success"] = report["real_router"]["success"]
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"success": report["success"], "checks": len(report["checks"]),
                      "report": str(args.report)}, ensure_ascii=False))
    raise SystemExit(0 if report["success"] else 1)


if __name__ == "__main__":
    main()
