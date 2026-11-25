#!/usr/bin/env python3
"""Run real legacy clients against the already-built production account router.

The official v0.10.13 image is pinned by its verified release ELF digest.
An optional observed v0.10.12 image is reported separately without an official
provenance claim. Every child uses owned HOME/auth/config/session directories.
This fixture never builds, installs, reloads an existing client, or starts 007.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import subprocess
import sys
import tempfile
import time

from e2e import Harness
from e2e_account_gateway import private_json


OFFICIAL_V13_SHA256 = "5ad6682a2e63e865b9a0ada78ce0d1b9b7799cf95e6bdad15c4661ecc106a7aa"


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def connection_line(process, label, timeout_seconds=12, max_bytes=16384):
    """Read one bounded Ready line; readable partial output must not disable timeout."""
    descriptor = process.stdout.fileno()
    os.set_blocking(descriptor, False)
    received = bytearray()
    deadline = time.monotonic() + timeout_seconds
    try:
        with selectors.DefaultSelector() as selector:
            selector.register(descriptor, selectors.EVENT_READ)
            while True:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise AssertionError(label + ": no complete connection record before deadline")
                chunk = os.read(descriptor, min(4096, max_bytes + 1 - len(received)))
                if not chunk:
                    raise AssertionError(label + ": client ended before its complete Ready record")
                received.extend(chunk)
                if len(received) > max_bytes:
                    raise AssertionError(label + ": oversized connection record")
                newline = received.find(b"\n")
                if newline >= 0:
                    return bytes(received[:newline + 1])
    finally:
        os.set_blocking(descriptor, True)


def private_directory(path):
    path.mkdir(mode=0o700)
    return path


def isolated_environment(root, path):
    roots = {name: private_directory(root / name)
             for name in ("home", "config", "data", "cache", "auth", "sessions")}
    config = root / "client.toml"
    config.write_text("")
    config.chmod(0o600)
    return {
        "PATH": str(path), "HOME": str(roots["home"]),
        "XDG_CONFIG_HOME": str(roots["config"]), "XDG_DATA_HOME": str(roots["data"]),
        "XDG_CACHE_HOME": str(roots["cache"]), "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8",
        "TERM": "dumb", "NO_COLOR": "1", "ENV": "/dev/null", "BASH_ENV": "/dev/null",
        "NO_PROXY": "127.0.0.1,localhost,::1",
        "SSMCP_AUTH_HOME": str(roots["auth"]), "SSMCP_SESSION_HOME": str(roots["sessions"]),
        "SSMCP_CLIENT_CONFIG": str(config), "SSMCP_UI_CONFIG": str(config),
    }


class LegacyHarness(Harness):
    def __init__(self, directory, binaries, fixture_binary, server_revision):
        super().__init__(directory, binaries[0]["directory"])
        self.binaries = binaries
        self.fixture_binary = fixture_binary
        self.server_revision = server_revision
        self.fixture_timeout_seconds = 180
        fixture_home = private_directory(directory / "router-environment")
        self.fixture_environment = isolated_environment(fixture_home, os.defpath)
        self.other_token = ""
        self.cases = []

    def spawn(self, label, command, **kwargs):
        kwargs.setdefault("cwd", self.directory)
        return super().spawn(label, command, **kwargs)

    def client_environment(self, label, **_):
        directory = private_directory(self.directory / (label + "-bin"))
        for name in ("stty", "tty", "sleep", "head", "tr"):
            executable = shutil.which(name)
            if not executable:
                raise RuntimeError(f"test requires system {name}")
            (directory / name).symlink_to(Path(executable).resolve())
        (directory / "python3").symlink_to(Path(sys.executable).resolve())
        state = private_directory(self.directory / (label + "-state"))
        environment = isolated_environment(state, directory)
        root = Path(environment["SSMCP_AUTH_HOME"])
        private_json(root / (hashlib.sha256(self.origin.encode()).hexdigest() + ".json"), {
            "server": self.origin, "token": self.account_token,
            "principal": self.identity["principal"], "subject": self.identity["subject"],
            "username": self.identity["username"], "expires_at": self.account_expires_at,
        })
        selected = root / "default"
        with selected.open("x", encoding="utf-8") as stream:
            os.chmod(selected, 0o600)
            stream.write(self.origin)
        return environment

    def client_command(self, *, name="workstation", shell="/bin/sh", extra_args=()):
        return self.account_gateway.client_command(
            name=name, shell=shell, extra_args=("--no-browser", *extra_args), legacy=True)

    def native(self, client_id, name, arguments):
        status, result, _ = self.request("POST", {
            "client_id": client_id, "name": name, "arguments": arguments,
        }, secret=self.account_token, path="/api/client/call")
        if status != 200 or not isinstance(result, dict) or result.get("isError"):
            raise AssertionError(f"owned native call {name} failed with HTTP {status}")
        return result.get("structuredContent", result)

    def sessions(self, token):
        status, result, _ = self.request(
            "GET", secret=token, path="/api/client/sessions")
        if status != 200 or not isinstance(result, dict):
            raise AssertionError("fixture account cannot list its own sessions")
        return result["clients"]

    def run_case(self, binary, explicit):
        self.binary_dir = binary["directory"]
        mode = "explicit-self" if explicit else "default-self"
        label = binary["label"] + "-" + mode
        environment = self.client_environment(label)
        workspace = private_directory(self.directory / (label + "-workspace"))
        extra = ("--allow", self.identity["principal"]) if explicit else ()
        process = self.spawn(
            label, self.client_command(name=label, extra_args=extra),
            stdout=subprocess.PIPE, cwd=workspace, env=environment,
        )
        # The production client emits its Ready record on redirected stdout.
        line = connection_line(process, label)
        credentials = self.validate_connection_record(json.loads(line))
        client_id = credentials["client_id"]
        self.check(credentials.get("allow") == [self.identity["principal"]],
                   label + ": actual legacy client publishes its single provider-style self principal")
        self.check(any(item["id"] == client_id for item in self.sessions(self.account_token)),
                   label + ": owner lists the real connected legacy client")
        self.check(not any(item["id"] == client_id for item in self.sessions(self.other_token)),
                   label + ": another verified account cannot list the owner's client")
        denied, result, _ = self.request("POST", {
            "client_id": client_id, "name": "terminal.status", "arguments": {},
        }, secret=self.other_token, path="/api/client/call")
        self.check(denied == 404 and result.get("error") == "client_unavailable",
                   label + ": another verified account cannot call the owner's native tools")
        terminal = self.native(client_id, "terminal.status", {})
        self.check(isinstance(terminal, dict),
                   label + ": Ready is followed by a real native terminal response")
        job = self.native(client_id, "ssmcp.exec.start", {
            "command": ["/bin/sh", "-c", "printf legacy-self-ok"], "timeout_ms": 10000,
        })
        self.native(client_id, "ssmcp.exec.wait", {"job_id": job["job_id"], "wait_ms": 5000})
        output = self.native(client_id, "ssmcp.exec.read", {"job_id": job["job_id"], "max_bytes": 4096})
        self.check(output.get("exit_code") == 0 and output.get("stdout") == "legacy-self-ok",
                   label + ": authenticated relay executes a real owned shell command")
        time.sleep(1.2)
        matches = [item for item in self.sessions(self.account_token) if item["name"] == label]
        self.check(process.poll() is None and [item["id"] for item in matches] == [client_id],
                   label + ": the same connection remains live after the retry interval")
        self.native(client_id, "terminal.status", {})
        for log_path, log in self.logs:
            if log_path.name == label + ".log":
                log.flush()
                text = log_path.read_text(errors="replace")
                self.check("Disconnected; retrying" not in text,
                           label + ": no abnormal watchdog retry occurred")
                break
        self.stop(process)
        deadline = time.monotonic() + 5
        while any(item["id"] == client_id for item in self.sessions(self.account_token)):
            if time.monotonic() >= deadline:
                raise AssertionError(label + ": owned test connection remained registered after cleanup")
            time.sleep(0.02)
        self.check(not any(item["id"] == client_id for item in self.sessions(self.account_token)),
                   label + ": only the owned test connection is removed during cleanup")
        self.cases.append({"client": binary["label"], "mode": mode, "status": "passed"})

    def run(self):
        self.prepare()
        ready = json.loads((self.account_gateway.directory / "ready.json").read_text())
        other = ready["other_identity"]
        self.other_token = ready["other_account_token"]
        self.sensitive.append(self.other_token)
        self.check(other["subject"] != self.identity["subject"]
                   and other["principal"] != self.identity["principal"]
                   and other["username"] != self.identity["username"]
                   and self.other_token != self.account_token,
                   "the real router fixture has two independently registered identities")
        status, identity, _ = self.request(
            "GET", secret=self.other_token, path="/api/client/whoami")
        self.check(status == 200 and identity.get("identity") == other,
                   "the second credential authenticates as a different real fixture account")
        for binary in self.binaries:
            for explicit in (False, True):
                self.run_case(binary, explicit)
        return {
            "status": "passed", "checks": len(self.checks), "passed_checks": self.checks,
            "server_revision": self.server_revision, "fixture_sha256": digest(self.fixture_binary),
            "clients": [{key: value for key, value in binary.items() if key != "directory"}
                        for binary in self.binaries],
            "cases": self.cases,
            "transport": "real production Broker/router, account HTTP and WebSocket",
            "upstream_provider": "synthetic GitHub fixture",
            "real_user_credentials_used": False, "models_started": False,
            "existing_clients_reloaded": False, "existing_services_touched": False,
            "scope": "legacy self handshake, real native call, cross-identity denial and stable connection",
            "not_claimed": "preserving native jobs across a server deployment or testing server rejection with CLI-prevalidated foreign allow lists",
        }


def copied_client(directory, label, source, expected_version, expected_sha256, provenance):
    source = source.resolve(strict=True)
    if not source.is_file() or not os.access(source, os.X_OK):
        raise ValueError(label + ": executable input is required")
    before = digest(source)
    if expected_sha256 and before != expected_sha256:
        raise ValueError(label + ": input ELF digest differs from the pinned input")
    binary_dir = private_directory(directory / label)
    target = binary_dir / "ssmcp"
    shutil.copyfile(source, target)
    target.chmod(0o555)
    if digest(target) != before or digest(source) != before:
        raise AssertionError(label + ": the source or private copy changed")
    verification = private_directory(directory / (label + "-verification"))
    environment = isolated_environment(verification, os.defpath)
    version = subprocess.run(
        [str(target), "--version"], stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        text=True, env=environment, cwd=verification, timeout=5, check=True,
    ).stdout.strip()
    if version != "ssmcp " + expected_version:
        raise ValueError(label + ": unexpected executable version")
    return {"label": label, "directory": binary_dir, "version": version,
            "sha256": before, "provenance": provenance}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--official-v13", required=True, type=Path)
    parser.add_argument("--runtime-v12", type=Path)
    parser.add_argument("--runtime-v12-sha256")
    parser.add_argument("--fixture-binary", required=True, type=Path)
    parser.add_argument("--server-revision", required=True)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.server_revision):
        parser.error("--server-revision must be the frozen server commit")
    if bool(args.runtime_v12) != bool(args.runtime_v12_sha256):
        parser.error("provide --runtime-v12 and --runtime-v12-sha256 together")
    if args.runtime_v12_sha256 and not re.fullmatch(r"[0-9a-f]{64}", args.runtime_v12_sha256):
        parser.error("--runtime-v12-sha256 must be a complete SHA-256")
    fixture = args.fixture_binary.resolve(strict=True)
    fixture_before = digest(fixture)
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-legacy-compat-") as temporary:
        directory = Path(temporary)
        binaries = [copied_client(
            directory, "official-v13", args.official_v13, "0.10.13", OFFICIAL_V13_SHA256,
            "verified official v0.10.13 release ELF; invoked only as a normal client",
        )]
        if args.runtime_v12:
            binaries.append(copied_client(
                directory, "observed-v12", args.runtime_v12, "0.10.12", args.runtime_v12_sha256,
                "observed running v0.10.12 ELF; official release provenance not asserted",
            ))
        harness = LegacyHarness(directory, binaries, fixture, args.server_revision)
        try:
            result = harness.run()
        except Exception as error:
            harness.diagnostics()
            raise SystemExit(f"{type(error).__name__}: {error}") from None
        finally:
            harness.cleanup()
        if digest(fixture) != fixture_before:
            raise AssertionError("frozen server fixture changed during execution")
        result.update(elapsed_seconds=round(time.monotonic() - started, 3),
                      fixture_exit_code=harness.fixture_exit_code,
                      script_sha256=digest(Path(__file__)))
        if args.report:
            args.report.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
        print(json.dumps(result, ensure_ascii=False))


if __name__ == "__main__":
    main()
