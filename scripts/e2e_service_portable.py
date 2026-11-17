#!/usr/bin/env python3
"""Portable client service acceptance without native installation or a broker.

Runs a real foreground supervisor in a fresh SSMCP_SESSION_HOME. Every SSH
attempt is restricted to 127.0.0.1:9, with an explicit config disabling private
keys, agents, proxies and host-key writes. The handshake never reaches backend
discovery. No installed service, existing session or model account is changed.

Usage:
    python scripts/e2e_service_portable.py --bin-dir target/debug \
        --report target/portable-service.json
"""
from __future__ import annotations

import argparse
import base64
import concurrent.futures
import configparser
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import time


def wait_for(predicate, description, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError(f"timed out waiting for {description}")


class Portable:
    def __init__(self, binary: Path, root: Path, artifacts: Path):
        self.binary = binary.resolve()
        self.root = root
        self.workspace = root / "workspace unicode 中"
        self.workspace.mkdir()
        self.home = root / "sessions"
        # The supervisor creates its state root with the platform-private ACL.
        self.artifacts = artifacts
        self.environment = dict(os.environ)
        self.environment.update(SSMCP_SESSION_HOME=str(self.home), NO_COLOR="1")
        self.environment.pop("SSH_AUTH_SOCK", None)
        self.environment.pop("SSH_AGENT_PID", None)
        self.manager = None
        self.manager_logs = []
        self.names = set()
        self.secrets = []
        self.checks = []
        self.samples = {}
        self.config = root / "fixture-ssh.conf"
        hosts = root / "empty-known-hosts"
        hosts.write_text("", encoding="utf-8")
        host_path = str(hosts).replace("\\", "/").replace('"', '\\"')
        self.config.write_text(
            "Host *\n"
            "    HostName 127.0.0.1\n"
            "    Port 9\n"
            "    User ssmcp-portable-fixture\n"
            "    BatchMode yes\n"
            "    StrictHostKeyChecking yes\n"
            f'    UserKnownHostsFile "{host_path}"\n'
            f'    GlobalKnownHostsFile "{host_path}"\n'
            "    IdentityFile none\n"
            "    IdentityAgent none\n"
            "    IdentitiesOnly yes\n"
            "    PKCS11Provider none\n"
            "    ProxyCommand none\n"
            "    ProxyJump none\n"
            "    CanonicalizeHostname no\n"
            "    ForwardAgent no\n"
            "    ForwardX11 no\n"
            "    ClearAllForwardings yes\n"
            "    ControlMaster no\n"
            "    ControlPath none\n",
            encoding="utf-8",
        )

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
            "sessions", "create", name, "--json", "--watchdog",
            "--reconnect-delay", "1", "--no-setup", "--title",
            "portable lifecycle fixture; closed loopback SSH",
            "--cwd", str(self.workspace), "-F", str(self.config),
            "-p", "9", "ssmcp-portable-fixture@127.0.0.1",
        ]

    def create(self, name, *, expected=0):
        self.names.add(name)
        return self.cli(self.create_args(name), expected=expected)

    def failed_connection(self, name):
        def observed():
            value = self.action("status", name)
            if value.get("connection_state") == "connected":
                raise AssertionError("closed-loopback fixture unexpectedly connected")
            if value.get("attempt", 0) > 0 and value.get("last_error"):
                return value
            return False

        value = wait_for(observed, f"failed loopback SSH observation for {name}")
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
        # Refuse to interact even with an unrelated local listener on this port.
        with socket.socket() as probe:
            probe.settimeout(0.3)
            if probe.connect_ex(("127.0.0.1", 9)) == 0:
                raise RuntimeError("127.0.0.1:9 is in use; closed-loopback fixture is unavailable")
        if shutil.which("ssh") is None and os.name != "nt":
            raise RuntimeError("OpenSSH ssh is required for the failing transport fixture")
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
        self.cli(["attach", "absent-fixture", "--read-only"], expected="failure")
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

    def diagnostics(self):
        for index, log in enumerate(self.manager_logs):
            if log.is_file():
                value = self.redact(log.read_text(encoding="utf-8", errors="replace")[-32768:])
                (self.artifacts / f"portable-manager-{index}.log").write_text(value, encoding="utf-8")

    def cleanup(self):
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
