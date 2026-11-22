#!/usr/bin/env python3
"""Verify explicit self-account compatibility against real isolated old/new daemons.

Only generated accounts and tokens are used. No production state, OAuth provider,
installed client, or global user configuration is read or changed.
"""
import argparse
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import signal
import socket
import struct
import sys
import threading
import subprocess
import tempfile
import time

SOURCE = Path(__file__).resolve().parents[1]
OLD_COMMIT = "4e3132d60600dca5053af81cdefa6962c3058ac4"
OLD_RUNNER_BLOB_ID = "8d15b56207461ee817635c6b134380e1c2ffdc4d"
OLD_RUNNER_SHA256 = "6e169387073dfb5919f6714658ac28781cfe04c7e5800ada690c0c67fed3d4d0"


def old_runner_bytes(provided=None):
    if provided is not None:
        blob = provided.read_bytes()
    else:
        try:
            blob = subprocess.check_output(
                ["git", "show", OLD_COMMIT + ":src/code_mode.js"], cwd=SOURCE,
                stderr=subprocess.PIPE,
            )
        except (OSError, subprocess.CalledProcessError) as error:
            raise RuntimeError(
                "original 0.10.13 runner is unavailable; pass --old-runner with "
                "the verified byte-identical code_mode.js from the cleaned source"
            ) from error
    object_id = hashlib.sha1(b"blob " + str(len(blob)).encode("ascii") + b"\0" + blob).hexdigest()
    if object_id != OLD_RUNNER_BLOB_ID or hashlib.sha256(blob).hexdigest() != OLD_RUNNER_SHA256:
        raise RuntimeError("old runner differs from the pinned 0.10.13 Git blob and SHA256")
    return blob


def private(path, value):
    path.parent.mkdir(parents=True, exist_ok=True, mode=0o700)
    path.write_text(value)
    path.chmod(0o600)


def compatible_account_startup(before, after):
    """Permit only assigning a generated name to a legacy unnamed account."""
    if set(before) != set(after) or set(before["accounts"]) != set(after["accounts"]):
        return False
    normalized = dict(after)
    normalized["accounts"] = {subject: dict(account) for subject, account in after["accounts"].items()}
    for subject, old in before["accounts"].items():
        account = normalized["accounts"][subject]
        name = account.get("handle")
        if old.get("handle") is None and name is not None:
            if (not isinstance(name, str) or len(name) != 23 or not name.startswith("member-")
                    or any(c not in "0123456789abcdef" for c in name[7:])
                    or account.get("generated_handle") is not True):
                return False
            account["handle"] = None
            account.pop("generated_handle")
    return normalized == before


def wait_for(predicate, description, timeout=20):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.05)
    raise AssertionError("timed out: " + description)


class WirePeer:
    """Authenticated-controller tests use an anonymous synthetic controlled stream."""
    def __init__(self, port):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=20)
        self.buffer = b""
        key = base64.b64encode(os.urandom(16)).decode()
        headers = [
            "GET /api/client/connect HTTP/1.1", "Host: ssmcp-compat.invalid",
            "Connection: Upgrade", "Upgrade: websocket", "Sec-WebSocket-Version: 13",
            "Sec-WebSocket-Key: " + key,
            "ssmcp-capabilities: session_access_v1,control_attribution_v1",
        ]
        self.sock.sendall(("\r\n".join(headers) + "\r\n\r\n").encode())
        while b"\r\n\r\n" not in self.buffer:
            data = self.sock.recv(4096)
            if not data:
                raise RuntimeError("anonymous wire handshake closed")
            self.buffer += data
            if len(self.buffer) > 16384:
                raise RuntimeError("oversize wire handshake")
        header, self.buffer = self.buffer.split(b"\r\n\r\n", 1)
        if int(header.split(b"\r\n", 1)[0].split()[1]) != 101:
            raise RuntimeError("anonymous wire upgrade rejected")

    def send(self, value, opcode=1):
        data = value if isinstance(value, bytes) else json.dumps(value).encode()
        mask = os.urandom(4)
        size = bytes([0x80 | len(data)]) if len(data) < 126 else b"\xfe" + struct.pack("!H", len(data))
        self.sock.sendall(bytes([0x80 | opcode]) + size + mask
                          + bytes(value ^ mask[i % 4] for i, value in enumerate(data)))

    def exact(self, count):
        while len(self.buffer) < count:
            data = self.sock.recv(max(4096, count - len(self.buffer)))
            if not data:
                raise RuntimeError("wire peer closed")
            self.buffer += data
        result, self.buffer = self.buffer[:count], self.buffer[count:]
        return result

    def frame(self):
        while True:
            first, second = self.exact(2)
            opcode, size = first & 15, second & 127
            if size == 126:
                size = struct.unpack("!H", self.exact(2))[0]
            elif size == 127:
                size = struct.unpack("!Q", self.exact(8))[0]
            if size > 1024 * 1024 or second & 128:
                raise RuntimeError("invalid server wire frame")
            data = self.exact(size)
            if opcode == 9:
                self.send(data, 10)
            elif opcode in (1, 2):
                return json.loads(data)
            elif opcode == 8:
                raise RuntimeError("server closed wire peer")

    def close(self):
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.sock.close()


class Fixture:
    def __init__(self, root, binary, client, label):
        self.root, self.binary, self.client, self.label = root, binary, client, label
        self.checks, self.processes, self.logs = [], [], []
        self.tokens = {subject: "cl_" + secrets.token_hex(32) for subject in ("123", "124", "125")}
        self.expires = int(time.time()) + 3600
        probe = socket.socket()
        probe.bind(("127.0.0.1", 0))
        self.port = probe.getsockname()[1]
        probe.close()
        self.origin = f"http://127.0.0.1:{self.port}"
        self.public_origin = "https://ssmcp-compat.invalid"
        self.allowed = "self-fixture@" + self.origin
        self.other_allowed = "other-fixture@" + self.origin
        self.base_env = {k: v for k, v in os.environ.items() if not k.startswith("SSMCP_")}
        self.server = None

    def check(self, name, condition, **details):
        record = {"check": self.label + ":" + name, "passed": bool(condition), **details}
        self.checks.append(record)
        print(json.dumps(record), flush=True)
        if not condition:
            raise AssertionError(record["check"])

    def redact(self, text):
        for token in self.tokens.values():
            text = text.replace(token, "[synthetic-token]")
        return text

    def start(self):
        portal = self.root / "portal"
        portal.mkdir(mode=0o700)
        accounts = {}
        for subject, handle in [("123", "self-fixture"), ("124", "other-fixture"), ("125", None)]:
            accounts[subject] = {
                "subject": subject, "username": "gh-" + subject,
                "github_login": "fixture-" + subject, "keys": [],
                "handle": handle, "banned": False,
            }
        private(portal / "accounts.json", json.dumps({"accounts": accounts}))
        self.accounts_before = (portal / "accounts.json").read_bytes()
        records = {
            hashlib.sha256(token.encode()).hexdigest(): {
                "subject": subject, "kind": "login", "expires_at": self.expires,
                "parent": None, "session_ttl_secs": 0,
            }
            for subject, token in self.tokens.items()
        }
        private(portal / "client-credentials.json", json.dumps(records))
        config = self.root / "server.toml"
        private(config, (
            f'[server]\nlisten = "127.0.0.1:{self.port}"\npublic_url = "{self.public_origin}"\n'
            f'socket = {json.dumps(str(self.root / "control.sock"))}\n'
            '[auth.github]\nclient_id = "isolated-compat-fixture"\n'
            'client_secret = "not-a-real-provider-secret"\n'
            f'[oauth]\nclient_registry = {json.dumps(str(self.root / "oauth-clients.toml"))}\n'
            f'[portal]\nstate_dir = {json.dumps(str(portal))}\n'
            '[commerce]\nenabled = false\n'
        ))
        log = (self.root / "server.log").open("wb")
        self.logs.append(log)
        self.server = subprocess.Popen(
            [str(self.binary), "serve", "--config", str(config)],
            stdout=log, stderr=subprocess.STDOUT, env=self.base_env, start_new_session=True,
        )
        self.processes.append(self.server)

        def ready():
            if self.server.poll() is not None:
                raise AssertionError("daemon exited: " + self.redact(
                    (self.root / "server.log").read_text(errors="replace")[-1800:]))
            try:
                with socket.create_connection(("127.0.0.1", self.port), timeout=.1):
                    return True
            except OSError:
                return False
        wait_for(ready, "isolated daemon")
        status, body = self.api("GET", "/api/client/whoami", "123")
        self.check("real_daemon_verifies_synthetic_subject_and_handle",
                   status == 200 and body["identity"]["subject"] == "123"
                   and body["identity"]["username"] == "self-fixture")
        after_startup = (portal / "accounts.json").read_bytes()
        self.check("startup_preserves_accounts_except_generated_default_names",
                   compatible_account_startup(json.loads(self.accounts_before), json.loads(after_startup)))
        # Future compatibility requests must remain read-only, after any startup migration.
        self.accounts_before = after_startup

    def api(self, method, path, subject=None, body=None, *, capture_invalid_json=False):
        connection = http.client.HTTPConnection("127.0.0.1", self.port, timeout=20)
        headers = {"Content-Type": "application/json"}
        if subject is not None:
            headers["Authorization"] = "Bearer " + self.tokens[subject]
        try:
            connection.request(method, path, body=None if body is None else json.dumps(body), headers=headers)
            response = connection.getresponse()
            payload = response.read()
            try:
                value = json.loads(payload) if payload else None
            except (ValueError, UnicodeError):
                if not capture_invalid_json:
                    raise
                value = self.redact(payload.decode("utf-8", errors="replace"))[:2500]
            return response.status, value
        finally:
            connection.close()

    def client_environment(self, name, subject="123", **overrides):
        directory = self.root / name
        directory.mkdir()
        auth = directory / "auth"
        auth.mkdir(mode=0o700)
        (directory / "empty-path").mkdir()
        credential = {
            "server": self.origin, "token": self.tokens[subject],
            "principal": f"fixture-{subject}@github", "subject": subject,
            "username": "self-fixture" if subject == "123" else "other-fixture",
            "expires_at": self.expires,
        }
        if "cached_subject" in overrides:
            credential["subject"] = overrides.pop("cached_subject")
        credential.update(overrides)
        path = auth / (hashlib.sha256(self.origin.encode()).hexdigest() + ".json")
        private(path, json.dumps(credential))
        private(auth / "default", self.origin)
        env = dict(self.base_env)
        env.update(
            PATH=str(directory / "empty-path"),
            XDG_CONFIG_HOME=str(directory / "config"),
            XDG_STATE_HOME=str(directory / "state"),
            XDG_CACHE_HOME=str(directory / "cache"),
            XDG_DATA_HOME=str(directory / "data"),
            SSMCP_AUTH_HOME=str(auth),
            SSMCP_SESSION_HOME=str(directory / "sessions"),
            NO_COLOR="1", TERM="dumb", SHELL="/bin/sh",
        )
        return directory, env, path

    def args(self, *extra):
        return [
            str(self.client), "--server", self.origin, "--shell", "/bin/sh",
            "--no-setup", "--no-browser", "--log-format", "jsonl", *extra,
        ]

    def rejected(self, name, extra, *, subject="123", missing=False, **overrides):
        directory, env, path = self.client_environment(name, subject, **overrides)
        if missing:
            path.unlink()
        before = path.read_bytes() if path.exists() else None
        result = subprocess.run(
            self.args("--no-watchdog", *extra), cwd=directory, env=env,
            stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=20,
        )
        text = self.redact(result.stdout + result.stderr)
        self.check(name, result.returncode != 0 and '"client_id":' not in text,
                   exit_code=result.returncode)
        self.check(name + "_does_not_change_credentials",
                   (path.read_bytes() if path.exists() else None) == before)
        return text

    def spawn(self, name, allow, compat=False, bare=False, **overrides):
        directory, env, path = self.client_environment(name, **overrides)
        before = path.read_bytes()
        stdout = directory / "stdout.log"
        stderr = directory / "stderr.log"
        out, err = stdout.open("wb"), stderr.open("wb")
        self.logs.extend([out, err])
        args = self.args("--name", name, "--allow", allow, "--reconnect-delay", "1")
        if compat:
            args.append("--account-compat")
        if bare:
            args = [str(self.client)]
        process = subprocess.Popen(
            args, cwd=directory, env=env, stdin=subprocess.DEVNULL,
            stdout=out, stderr=err, start_new_session=True,
        )
        self.processes.append(process)

        def connection():
            if process.poll() is not None:
                raise AssertionError("client exited: " + self.redact(
                    (stdout.read_text(errors="replace") + stderr.read_text(errors="replace"))[-2400:]))
            for line in stdout.read_text(errors="replace").splitlines():
                try:
                    value = json.loads(line)
                except ValueError:
                    continue
                if isinstance(value, dict) and "client_id" in value and (bare or value.get("name") == name):
                    return value
            return None
        info = wait_for(connection, name + " backend-ready connection", timeout=30)
        self.check(name + "_keeps_saved_credential",
                   json.loads(path.read_bytes()) == json.loads(before) if bare else path.read_bytes() == before)
        return process, info, stdout, stderr, directory

    def call(self, subject, client_id, directory, marker):
        status, result = self.api("POST", "/api/client/call", subject, {
            "client_id": client_id, "name": "ssmcp.exec.start",
            "arguments": {"command": ["/bin/printf", marker], "cwd": str(directory)},
        })
        if status != 200:
            return status, result
        data = result.get("structuredContent", result)
        job_id = data.get("job_id") or data.get("id")
        if not job_id:
            return status, result
        output = b""
        cursor = 0
        for _ in range(20):
            status, result = self.api("POST", "/api/client/call", subject, {
                "client_id": client_id, "name": "ssmcp.exec.read",
                "arguments": {"job_id": job_id, "stdout_cursor": cursor, "wait_ms": 1000, "max_bytes": 1024},
            })
            if status != 200:
                return status, result
            data = result.get("structuredContent", result)
            output += base64.b64decode(data.get("stdout_base64", ""), validate=True)
            cursor = data.get("stdout_cursor", cursor)
            snapshot = data.get("snapshot", {})
            if snapshot.get("running") is False or snapshot.get("state") in ("completed", "failed"):
                return status, {"stdout": output.decode(errors="replace"), "snapshot": {
                    key: snapshot.get(key) for key in ("state", "running", "exit_code", "timed_out")
                }}
        raise AssertionError("synthetic native job did not finish")

    def self_only(self):
        args = ["--account-compat", "--allow", self.allowed]
        self.rejected("missing_saved_login", args, missing=True)
        self.rejected("expired_saved_login", args, expires_at=int(time.time()) - 1)
        self.rejected("wrong_cached_subject", args, cached_subject="124")
        self.rejected("wrong_cached_principal", args, principal="different@github")
        self.rejected("different_allowed_account",
                      ["--account-compat", "--allow", self.other_allowed])
        self.rejected("multiple_accounts",
                      ["--account-compat", "--allow", self.allowed + "," + self.other_allowed])
        self.rejected("cross_origin", ["--account-compat", "--allow", "self-fixture@other.invalid"])
        self.rejected("missing_registered_handle",
                      ["--account-compat", "--allow", "unknown-fixture@" + self.origin], subject="125")
        process, info, stdout, stderr, directory = self.spawn(
            "compat-self", self.allowed, compat=True,
        )
        client_id = info["client_id"]
        self.check("account_mode_is_explicit_in_connection_output",
                   info.get("account_compat") is True
                   and info.get("ownership") == "verified_account"
                   and info.get("quota") == "account"
                   and info.get("allow") == [self.allowed]
                   and "login_code" not in info)
        logs = stdout.read_text(errors="replace") + stderr.read_text(errors="replace")
        self.check("ownership_and_quota_notice_is_visible",
                   "owns this connection" in logs and "account quota" in logs)
        status, result = self.call("123", client_id, directory, "compat-owner-ok")
        self.check("owner_executes_real_native_tool", status == 200 and "compat-owner-ok" in json.dumps(result), status=status, response=self.redact(json.dumps(result))[:2500])
        status, result = self.api("GET", "/api/client/sessions", "124")
        self.check("another_subject_cannot_list_connection", status == 200 and result.get("clients") == [],
                   status=status, response=self.redact(json.dumps(result))[:2500],
                   target_client_id=self.redact(str(client_id))[:128])
        status, _ = self.call("124", client_id, directory, "MUST-NOT-RUN")
        self.check("another_subject_cannot_call_known_client_id", status == 404)
        status, _ = self.api("POST", "/api/client/logout", "123", {})
        self.check("synthetic_owner_revocation_accepted", status == 200)
        if self.label.startswith("old"):
            wait_for(lambda: process.poll() is not None, "revoked compatibility client exits", timeout=10)
            self.check("revoked_compatibility_exits_without_reauthorization", process.returncode != 0,
                       exit_code=process.returncode)
        else:
            whoami_status, _ = self.api("GET", "/api/client/whoami", "123")
            call_status, _ = self.call("123", client_id, directory, "MUST-NOT-RUN")
            self.check("revoked_controller_whoami_and_call_are_denied",
                       whoami_status == 401 and call_status == 401,
                       whoami_status=whoami_status, call_status=call_status,
                       controlled_socket_still_running=process.poll() is None)
            # Current relays deliberately keep controlled sockets independent
            # of controller logins. Close only this fixture's owned process.
            self.stop(process)
        self.rejected("revoked_saved_login", args)
        self.check("compatibility_never_claims_or_changes_accounts",
                   (self.root / "portal/accounts.json").read_bytes() == self.accounts_before)
        self.check("unrelated_account_and_daemon_remain_available",
                   self.server.poll() is None and self.api("GET", "/api/client/whoami", "124")[0] == 200)

    def default_self(self):
        process, info, stdout, stderr, directory = self.spawn(
            "bare-default", self.allowed, bare=True,
        )
        logs = stdout.read_text(errors="replace") + stderr.read_text(errors="replace")
        self.check("zero_argument_default_uses_same_verified_account",
                   info.get("account_compat") is True and info.get("allow") == [self.allowed]
                   and "login_code" not in info)
        self.check("zero_argument_default_retries_legacy_format_only_once",
                   logs.count("Older relay: reconnecting once") == 1)
        status, result = self.call("123", info["client_id"], directory, "bare-default-ok")
        self.check("zero_argument_default_executes_real_native_tool",
                   status == 200 and "bare-default-ok" in json.dumps(result), status=status)
        status, _ = self.call("124", info["client_id"], directory, "MUST-NOT-RUN")
        self.check("zero_argument_default_does_not_add_another_subject", status == 404)
        self.stop(process)

    def anonymous_multi(self):
        # The real daemon requires a public HTTPS origin. A raw local wire peer
        # exercises that exact public namespace without test-only TLS bypasses.
        peer = WirePeer(self.port)
        worker = None

        def join_worker():
            if worker is not None:
                worker.join(timeout=2)
                if worker.is_alive():
                    raise RuntimeError("anonymous peer worker did not exit after socket close")

        try:
            allow = ["self-fixture@ssmcp-compat.invalid", "other-fixture@ssmcp-compat.invalid"]
            peer.send({"type": "hello", "version": 7, "mode": "shell", "metadata": {
                "name": "anonymous-multi-wire", "cwd": str(self.root), "hostname": "fixture",
                "allow": allow,
            }})
            info = peer.frame()
            self.check("new_anonymous_multi_wire_policy_is_unchanged",
                       info.get("type") == "ready" and info.get("login_code") == ""
                       and info.get("login_expires_in") == 0
                       and set(info.get("capabilities", [])) ==
                       {"session_access_v1", "control_attribution_v1"},
                       anonymous_peer_client_id=self.redact(str(info.get("client_id")))[:128])

            def serve():
                try:
                    while True:
                        frame = peer.frame()
                        if frame.get("type") == "call":
                            peer.send({"type": "reply", "id": frame["id"], "result": {
                                "Ok": {"content": [{"type": "text", "text": "authorized-wire-reply"}]},
                            }})
                except (OSError, RuntimeError):
                    return

            worker = threading.Thread(target=serve, daemon=True)
            worker.start()
            for subject in ("123", "124"):
                status, result = self.api("POST", "/api/client/call", subject, {
                    "client_id": info["client_id"], "name": "fixture.echo", "arguments": {},
                })
                self.check("listed_subject_" + subject + "_wire_call_is_delivered",
                           status == 200 and "authorized-wire-reply" in json.dumps(result))
            status, _ = self.api("POST", "/api/client/call", "125", {
                "client_id": info["client_id"], "name": "fixture.echo", "arguments": {},
            })
            self.check("unlisted_subject_wire_call_is_denied", status == 404)
        finally:
            self.cleanup("anonymous_peer_cleanup", peer.close, join_worker)

        # Local socket close is distinct from daemon retirement. The next
        # scenario requires this subject's entire session list to be empty.
        client_id = info["client_id"]
        details = {"anonymous_peer_client_id": self.redact(str(client_id))[:128]}

        def retired():
            status, result = self.api("GET", "/api/client/sessions", "124", capture_invalid_json=True)
            details.update(status=status, response=self.redact(json.dumps(result))[:2500])
            clients = result.get("clients") if isinstance(result, dict) else None
            if (status != 200 or not isinstance(clients, list)
                    or any(not isinstance(entry, dict)
                           or not isinstance(entry.get("id"), str) or not entry["id"]
                           for entry in clients)):
                raise ValueError("invalid session list while waiting for anonymous peer retirement")
            return all(entry["id"] != client_id for entry in clients)

        try:
            wait_for(retired, "anonymous peer retirement before self-only scenario")
        except Exception as error:
            details["error"] = self.redact(str(error))[:512]
            self.checks.append({"check": self.label + ":anonymous_peer_retirement", "passed": False, **details})
            error.add_note(json.dumps(details))
            raise

    @staticmethod
    def stop(process):
        if process.poll() is None:
            os.killpg(process.pid, signal.SIGTERM)
            try:
                process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)

    def cleanup(self, name, *steps):
        primary = sys.exception()
        errors = []
        for step in steps:
            try:
                step()
            except Exception as error:
                errors.append(self.redact(f"{type(error).__name__}: {error}")[:512])
        if errors:
            evidence = {"check": self.label + ":" + name, "passed": False,
                        "cleanup_error_count": len(errors), "cleanup_errors": errors[:4]}
            self.checks.append(evidence)
            if primary is not None:
                primary.add_note(json.dumps(evidence))
            else:
                raise RuntimeError(json.dumps(evidence))

    def close(self):
        self.cleanup("fixture_cleanup",
                     *(lambda process=process: self.stop(process) for process in reversed(self.processes)),
                     *(log.close for log in self.logs))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client", type=Path, required=True)
    parser.add_argument("--old-server", type=Path, required=True)
    parser.add_argument("--old-runner", type=Path,
                        help="verified 0.10.13 runner file; avoids requiring its original commit object")
    parser.add_argument("--new-server", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--work-dir", type=Path, default=Path("/var/tmp"))
    parser.add_argument("--check-default", action="store_true")
    parser.add_argument("--server-version", choices=("old", "new", "both"), default="both")
    args = parser.parse_args()
    for path in (args.client, args.old_server, args.new_server):
        if not path.is_file():
            parser.error("missing executable: " + str(path))
    report = {
        "schema": "ssmcp.account_compat.validation.v1", "passed": False,
        "production_state_used": False, "production_connections_opened": False,
        "global_configuration_modified": False, "checks": [],
        "client_sha256": hashlib.sha256(args.client.read_bytes()).hexdigest(),
        "old_server_sha256": hashlib.sha256(args.old_server.read_bytes()).hexdigest(),
        "new_server_sha256": hashlib.sha256(args.new_server.read_bytes()).hexdigest(),
    }
    started = time.monotonic()
    try:
        with tempfile.TemporaryDirectory(prefix="ssmcp-account-compat-", dir=args.work_dir) as temp:
            root = Path(temp)
            old = root / "old-release/bin/ssmcpd"
            old.parent.mkdir(parents=True)
            shutil.copy2(args.old_server, old)
            runner = root / "old-release/libexec/ssmcp/code-mode.js"
            runner.parent.mkdir(parents=True)
            legacy_runner = old_runner_bytes(args.old_runner)
            runner.write_bytes(legacy_runner)
            report.update(old_runner_build_commit=OLD_COMMIT,
                          old_runner_blob_id=OLD_RUNNER_BLOB_ID,
                          old_runner_sha256=hashlib.sha256(legacy_runner).hexdigest())
            for label, server in (("old-0.10.13", old), ("new-main", args.new_server.resolve())):
                if args.server_version != "both" and not label.startswith(args.server_version):
                    continue
                directory = root / label
                directory.mkdir(mode=0o700)
                fixture = Fixture(directory, server, args.client.resolve(), label)
                try:
                    fixture.start()
                    if label.startswith("old"):
                        fixture.rejected("default_allow_does_not_silently_authenticate",
                                         ["--allow", fixture.allowed])
                    else:
                        fixture.anonymous_multi()
                    if label.startswith("old") and args.check_default:
                        fixture.default_self()
                    fixture.self_only()
                finally:
                    try:
                        fixture.close()
                    finally:
                        report["checks"].extend(fixture.checks)
        report["passed"] = True
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        report["elapsed_seconds"] = round(time.monotonic() - started, 3)
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps({"report": str(args.report), "passed": report["passed"],
                          "checks": len(report["checks"])}), flush=True)


if __name__ == "__main__":
    main()
