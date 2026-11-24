#!/usr/bin/env python3
"""Verify default/custom local MCP Unix sockets with real OAuth and owned PTYs.

Uses installed binaries and the existing local fixture. No provider, browser,
model, remote account or desktop input is needed. Credentials stay in the private
temporary directory or memory; the report contains assertions and hashes only.
"""
from __future__ import annotations

import argparse
import http.client
import json
import os
from pathlib import Path
import socket
import stat
import subprocess
import tempfile
import threading
import time

from e2e_local import Local, sha256


class UnixHTTPConnection(http.client.HTTPConnection):
    def __init__(self, path, host, port, *, timeout):
        super().__init__(host, port, timeout=timeout)
        self.socket_path = path

    def connect(self):
        self.sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.sock.settimeout(self.timeout)
        self.sock.connect(str(self.socket_path))


class SocketLocal(Local):
    def __init__(self, directory, binary_dir, decoder, *, custom):
        self.custom = custom
        self.socket_path = None
        super().__init__(directory, binary_dir, decoder, False, None)
        if custom:
            parent = directory / "endpoint"
            parent.mkdir(mode=0o700)
            self.socket_path = parent / "mcp.sock"
            # Simulate a previous process that died after binding. Recovery must
            # use the normal listener checks, then serve real authenticated MCP.
            stale = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            stale.bind(str(self.socket_path))
            os.chmod(self.socket_path, 0o600)
            stale.close()

    def client_environment(self, label, **kwargs):
        environment = super().client_environment(label, **kwargs)
        if not self.custom:
            self.socket_path = Path(environment["XDG_RUNTIME_DIR"]) / "ssmcp/mcp.sock"
        return environment

    def client_command(self, **kwargs):
        command = super().client_command(**kwargs)
        return command + ["--socket"] + ([str(self.socket_path)] if self.custom else [])

    def request(self, method="POST", body=None, *, headers=None, secret=None, path=None):
        request_headers = {"Accept": "application/json, text/event-stream",
                           "Content-Type": "application/json"}
        token = self.secret if secret is None else secret
        if token:
            request_headers["Authorization"] = f"Bearer {token}"
        if headers:
            request_headers.update(headers)
        raw_body = body if isinstance(body, (bytes, str)) else json.dumps(body).encode()
        if body is None:
            raw_body = None
        connection = UnixHTTPConnection(self.socket_path, "127.0.0.1", self.http_port,
                                        timeout=self.http_timeout)
        try:
            connection.request(method, path or f"/{self.user}", body=raw_body, headers=request_headers)
            response = connection.getresponse()
            payload = response.read()
            try:
                result = json.loads(payload) if payload else None
            except json.JSONDecodeError:
                result = payload.decode(errors="replace")
            return response.status, result, dict(response.getheaders())
        finally:
            connection.close()
            if self.front is not None and threading.current_thread() is threading.main_thread():
                self.front.pump(0.01)

    def smoke(self):
        self.prepare()
        self.authorization_entry()
        metadata = self.socket_path.lstat()
        parent = self.socket_path.parent.stat()
        self.check(stat.S_ISSOCK(metadata.st_mode) and stat.S_IMODE(metadata.st_mode) == 0o600
                   and metadata.st_uid == os.geteuid(), "MCP socket is an owned 0600 Unix socket")
        self.check(stat.S_IMODE(parent.st_mode) == 0o700 and parent.st_uid == os.geteuid(),
                   "MCP socket parent is private and owned")
        self.check(self.request("GET", path="/healthz", secret="")[0] == 200,
                   "the advertised Unix socket serves actual HTTP")
        self.check(self.request(body={}, secret="")[0] == 401,
                   "same-UID Unix transport still requires MCP authentication")
        for headers in ({"Host": "attacker.invalid"}, {"Origin": "https://attacker.invalid"}):
            self.check(self.request("GET", path="/healthz", secret="", headers=headers)[0] == 403,
                       "the Unix listener preserves the canonical Host and Origin checks")
        competitor = subprocess.run(
            [str(self.executable), "local", "--socket", str(self.socket_path),
             "--no-watchdog", "--no-browser"],
            cwd=self.workspace, env=self.environments["local-fixture"], stdin=subprocess.DEVNULL,
            capture_output=True, text=True, timeout=10)
        self.front.pump(0.05)
        self.check(competitor.returncode != 0 and "already active" in competitor.stdout + competitor.stderr,
                   "a second real CLI refuses the active socket")
        self.check(self.socket_path.lstat().st_ino == metadata.st_ino
                   and self.request("GET", path="/healthz", secret="")[0] == 200,
                   "the original listener remains reachable after the collision")
        self.oauth_login(self.credentials)
        status, initialized, _ = self.rpc("initialize", {"protocolVersion": "2025-11-25",
            "capabilities": {}, "clientInfo": {"name": "unix-socket-acceptance", "version": "1"}})
        self.check(status == 200 and "result" in initialized,
                   "OAuth over the Unix socket initializes authenticated MCP")
        status, listed, _ = self.rpc("tools/list")
        self.check(status == 200 and bool(listed.get("result", {}).get("tools")),
                   "authenticated MCP lists tools over the same Unix socket")
        clients = self.public_call("list", {})["clients"]
        self.check(len(clients) == 1 and clients[0]["name"] == "local-fixture",
                   "MCP discovers the same-machine fixture session")
        self.client_id = clients[0]["id"]
        (self.workspace / "mcp-check.txt").write_text("local MCP verified")
        for attempt in (1, 2):
            read = self.call("ssmcp.fs.read_text", {"path": "mcp-check.txt"})
            self.check(read["text"] == "local MCP verified",
                       f"new MCP HTTP connection {attempt} reaches the same live native session")
        if self.custom:
            self.check(True, "the custom path recovered an owned stale socket and completed MCP")
        self.front.disconnect()
        self.check(not self.socket_path.exists(), "normal client shutdown removes its owned MCP socket")
        probe = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            probe.settimeout(1)
            try:
                probe.connect(str(self.socket_path))
            except (FileNotFoundError, ConnectionRefusedError):
                self.check(True, "the stopped socket no longer accepts connections")
            else:
                raise AssertionError("the stopped Unix socket still accepted a connection")
        finally:
            probe.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    args.bin_dir = args.bin_dir.resolve(strict=True)
    args.decoder = args.decoder.resolve(strict=True)
    if args.report.exists():
        parser.error("use a fresh report path to preserve earlier results")
    report = {"schema_version": 1, "ok": False, "transport": "HTTP/MCP over AF_UNIX",
              "source": sha256(__file__), "binary_sha256": sha256(args.bin_dir / "ssmcp"),
              "decoder_sha256": sha256(args.decoder), "cases": [],
              "external_account_used": False, "models_started": False}
    started = time.monotonic()
    try:
        with tempfile.TemporaryDirectory(prefix="ssmcp-sock-", dir="/tmp") as root:
            for label, custom in [("default", False), ("custom-stale", True)]:
                directory = Path(root) / label
                directory.mkdir(mode=0o700)
                harness = SocketLocal(directory, args.bin_dir, args.decoder, custom=custom)
                case = {"name": label, "ok": False, "checks": []}
                report["cases"].append(case)
                try:
                    harness.smoke()
                    case["ok"] = True
                finally:
                    case["checks"] = list(harness.checks)
                    harness.close()
        assert sha256(args.bin_dir / "ssmcp") == report["binary_sha256"]
        report["ok"] = True
    finally:
        report["seconds"] = round(time.monotonic() - started, 2)
        report["passed_checks"] = sum(len(case["checks"]) for case in report["cases"])
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(json.dumps({key: value for key, value in report.items() if key != "cases"}), flush=True)


if __name__ == "__main__":
    main()
