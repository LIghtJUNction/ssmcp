#!/usr/bin/env python3
"""Exercise 007 upgrades without calling systemd or changing any installed unit.

The manager is an explicit test double. Recovery and saved-session processes,
their executable files, WebSocket authentication, and connected observations
are real and use only the existing private synthetic-account fixture.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import socketserver
import subprocess
import sys
import tempfile
import threading
import time

from e2e_007 import Fixture, relay, worker_identity

UNIT = "ssmcp-007.service"


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def write_executable(path, source):
    path.write_text(source)
    path.chmod(0o700)


class Manager:
    """A private socket replaces only the systemctl calls made by this test."""
    def __init__(self, fixture):
        self.fixture = fixture
        self.lock = threading.RLock()
        self.commands = []
        self.fail_starts = 0
        self.stopped = False
        self.fragment = fixture.root / "loaded-recovery.service"
        self.unit = fixture.root / "config/systemd/user" / UNIT
        self.unit.parent.mkdir(parents=True, exist_ok=True)
        definition = self.definition(fixture.home)
        self.unit.write_text(definition)
        self.fragment.write_text(definition)
        self.unit.chmod(0o600)
        self.fragment.chmod(0o600)
        self.socket_path = fixture.root / "manager.sock"
        manager = self

        class Handler(socketserver.StreamRequestHandler):
            def handle(self):
                try:
                    line = self.rfile.readline(4097)
                    if len(line) > 4096:
                        raise ValueError("oversized manager request")
                    result = manager.call(json.loads(line))
                except Exception as error:
                    result = {"code": 98, "stdout": "", "error": type(error).__name__}
                self.wfile.write(json.dumps(result).encode() + b"\n")

        self.server = socketserver.ThreadingUnixStreamServer(str(self.socket_path), Handler)
        self.server.daemon_threads = True
        self.socket_path.chmod(0o600)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        commands = fixture.root / "upgrade-path"
        commands.mkdir(mode=0o700)
        write_executable(commands / "systemctl", f'''#!{sys.executable}
import json,socket,sys
with socket.socket(socket.AF_UNIX) as connection:
    connection.settimeout(20)
    connection.connect({str(self.socket_path)!r})
    connection.sendall(json.dumps(sys.argv[1:]).encode()+b"\\n")
    reply=json.loads(connection.makefile("rb").readline(65537))
sys.stdout.write(reply.get("stdout",""))
sys.stderr.write(reply.get("error", ""))
raise SystemExit(reply["code"])
''')
        self.environment = dict(fixture.environment, PATH=str(commands))

    @staticmethod
    def definition(home):
        def quote(value):
            return '"' + str(value).replace('\\', '\\\\').replace('"', '\\"').replace('%', '%%') + '"'
        return ("# ssmcp-007 managed user service\n[Service]\n"
                f"ExecStart={quote(home / 'ssmcp')} 007 run\n"
                f"Environment={quote('SSMCP_007_HOME=' + str(home))}\n")

    def call(self, args):
        if (not isinstance(args, list) or not all(isinstance(item, str) for item in args)
                or "--user" not in args or args.count(UNIT) != 1):
            return {"code": 97, "stdout": "", "error": "isolated manager rejects that target"}
        operations = [item for item in args if item in {"show", "start", "stop"}]
        allowed = {"--user", "--no-pager", UNIT, *operations}
        if len(operations) != 1 or any(item not in allowed and not item.startswith("--property=") for item in args):
            return {"code": 97, "stdout": "", "error": "isolated manager rejects that operation"}
        operation = operations[0]
        with self.lock:
            self.commands.append({"operation": operation, "unit": UNIT})
            process = self.fixture.zero
            running = process is not None and process.poll() is None
            if operation == "stop":
                if running:
                    process.send_signal(signal.SIGTERM)
                    process.wait(timeout=12)
                self.stopped = True
            elif operation == "start":
                if self.fail_starts:
                    self.fail_starts -= 1
                    return {"code": 17, "stdout": "", "error": "synthetic start failure"}
                if not running:
                    self.fixture.zero = self.fixture.spawn(
                        "upgrade-zero-" + str(len(self.fixture.processes)), "007", "run")
                self.stopped = False
            else:
                values = {
                    "LoadState": "loaded", "ActiveState": "active" if running else ("inactive" if self.stopped else "failed"),
                    "SubState": "running" if running else "dead", "UnitFileState": "enabled",
                    "MainPID": str(process.pid) if running else "0", "ExecMainStatus": "0",
                    "FragmentPath": str(self.fragment), "DropInPaths": "", "NeedDaemonReload": "no",
                    "MemoryCurrent": "0", "CPUUsageNSec": "0", "NRestarts": "0",
                }
                requested = next((item.partition("=")[2].split(",") for item in args
                                  if item.startswith("--property=")), list(values))
                return {"code": 0, "stdout": "".join(f"{key}={values[key]}\n" for key in requested)}
            return {"code": 0, "stdout": ""}

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)


def saved_files(fixture):
    paths = []
    for path in fixture.sessions.rglob("*"):
        if path.is_file() and (path.name == "manifest.json" or path.stat().st_mode & 0o100):
            paths.append(path)
    return {str(path.relative_to(fixture.sessions)): digest(path) for path in paths}


def run_fixture(binary):
    report = {"schema": "ssmcp.007.upgrade.e2e.v1", "success": False,
              "binary_sha256": digest(binary), "script_sha256": digest(__file__),
              "service_manager": "private systemctl test double; no real systemd commands or unit mutations",
              "transport": "real recovery and saved-session processes; private synthetic account WebSocket gateway",
              "real_credentials_used": False, "models_invoked": False, "checks": [],
              "same_binary_baseline": True, "live_007_changed": False}
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-007-upgrade-e2e-") as directory:
        fixture = Fixture(Path(directory), binary, native_systemd=False)
        fixture.snapshot.chmod(0o700)
        manager = None
        try:
            # Even an unexpected fallback must not invoke the user's real manager.
            guard_log = fixture.root / "blocked-systemd-probes.log"
            for name in ("systemctl", "systemd-run"):
                path = fixture.root / "minimal-path" / name
                path.unlink(missing_ok=True)
                write_executable(path, f'''#!{sys.executable}
with open({str(guard_log)!r},"a") as stream: stream.write({name!r}+"\\n")
raise SystemExit(1)
''')
            connection = fixture.start()
            manager = Manager(fixture)
            config_hash, unit_hash = digest(fixture.home / "config.json"), digest(manager.unit)

            def upgrade(environment=None, dry_run=False):
                args = [str(binary), "007", "upgrade", "--json"]
                if dry_run:
                    args.append("--dry-run")
                result = subprocess.run(args, cwd=fixture.workspace, env=environment or manager.environment,
                                        capture_output=True, text=True, timeout=185)
                value = json.loads(result.stdout) if result.stdout.strip() else None
                return result, value

            old_pid, old_id = fixture.zero.pid, connection.id
            old_hash = digest(fixture.snapshot)
            result, plan = upgrade(dry_run=True)
            fixture.check(result.returncode == 0 and plan["dry_run"] and plan["success"],
                          "dry-run reports the actual current CLI and private recovery process")
            fixture.check(fixture.zero.pid == old_pid and digest(fixture.snapshot) == old_hash
                          and not list(fixture.home.glob("ssmcp.backup-*"))
                          and all(item["operation"] == "show" for item in manager.commands),
                          "dry-run neither replaces an image nor stops a process")

            created = connection.tool("ssmcp.007.wake", {"name": "upgrade-survivor", "cwd": "child"})
            name = created.get("name", "upgrade-survivor")
            fixture.names.add(name)
            worker = fixture.worker_connected(name)
            retained = saved_files(fixture)
            fixture.check(any(path.endswith("manifest.json") for path in retained)
                          and len(retained) >= 2, "the real saved session has a definition and an executable snapshot")
            with fixture.state.lock:
                worker_socket = next(item for item in fixture.state.sessions.values()
                                     if item.hello["metadata"]["name"] == name)
            job = worker_socket.tool("ssmcp.exec.start", {"command": [sys.executable, "-c", "import time; time.sleep(240)"]})

            result, upgraded = upgrade()
            fixture.check(result.returncode == 0 and upgraded["success"]
                          and upgraded["new_pid"] == fixture.zero.pid != old_pid,
                          "successful upgrade reports the actual new owned process")
            observation = json.loads((fixture.home / "state.json").read_text())
            connection = fixture.connected_zero(previous=old_id)
            fixture.check(observation["state"] == "connected"
                          and observation["pid"] == fixture.zero.pid
                          and observation["client_id"] == connection.id
                          and observation["wire_version"] == 7,
                          "success matches a fresh authenticated v7 connection and observation")
            backup = Path(upgraded["backup"]["path"])
            fixture.check(backup.is_file() and digest(backup) == upgraded["backup"]["sha256"] == old_hash
                          and backup.stat().st_ino != fixture.snapshot.stat().st_ino,
                          "the previous image remains an independent, exact backup")
            fixture.check(digest(fixture.snapshot) == digest(binary) == upgraded["candidate_sha256"],
                          "the saved recovery image is byte-for-byte the invoked candidate")
            fixture.check(worker_identity(fixture.session_status(name)) == worker_identity(worker)
                          and saved_files(fixture) == retained
                          and worker_socket.tool("ssmcp.exec.status", {"job_id": job["job_id"]})["running"],
                          "upgrade preserves the formal session, connection, snapshot and ongoing job")
            fixture.check(digest(fixture.home / "config.json") == config_hash and digest(manager.unit) == unit_hash,
                          "upgrade preserves configuration and unit definition")

            # A private XDG file cannot redirect the already-loaded manager to another installation.
            other = fixture.root / "other"
            other.mkdir(mode=0o700)
            other_home, other_config = other / "007", other / "config"
            other_home.mkdir(mode=0o700)
            import shutil
            shutil.copy2(binary, other_home / "ssmcp")
            (other_home / "ssmcp").chmod(0o700)
            fixture.private_json(other_home / "config.json", fixture.config)
            other_unit = other_config / "systemd/user" / UNIT
            other_unit.parent.mkdir(parents=True)
            other_unit.write_text(Manager.definition(other_home))
            before = len(manager.commands)
            pid_before = fixture.zero.pid
            result, rejected = upgrade(dict(manager.environment, SSMCP_007_HOME=str(other_home), XDG_CONFIG_HOME=str(other_config)))
            fixture.check(result.returncode != 0 and rejected is None
                          and fixture.zero.pid == pid_before
                          and all(item["operation"] == "show" for item in manager.commands[before:])
                          and not list(other_home.glob("ssmcp.backup-*")),
                          "a different installation is rejected before backup, stop or replacement")

            # Fail exactly the replacement's first start; the rollback start is real.
            before_hash = digest(fixture.snapshot)
            before_pid = fixture.zero.pid
            manager.fail_starts = 1
            result, failed = upgrade()
            fixture.check(result.returncode != 0 and not failed["success"]
                          and failed["rollback"]["binary_restored"]
                          and failed["rollback"]["service_started"]
                          and failed["rollback"]["connected"],
                          "a failed activation exits unsuccessfully and separately confirms restoration and reconnection")
            observation = json.loads((fixture.home / "state.json").read_text())
            fixture.check(digest(fixture.snapshot) == before_hash and fixture.zero.pid != before_pid
                          and observation["pid"] == fixture.zero.pid and observation["state"] == "connected"
                          and Path(failed["backup"]["path"]).is_file() and backup.is_file(),
                          "rollback restores exact bytes, verifies its new process, and retains both backups")
            fixture.check(worker_identity(fixture.session_status(name)) == worker_identity(worker)
                          and saved_files(fixture) == retained
                          and worker_socket.tool("ssmcp.exec.status", {"job_id": job["job_id"]})["running"],
                          "rollback also leaves formal session identity and its ongoing job intact")
            fixture.check(all(item["unit"] == UNIT for item in manager.commands),
                          "every manager mutation targets only the private recovery unit")
            worker_socket.tool("ssmcp.exec.cancel", {"job_id": job["job_id"]})
            report["success"] = True
            report["manager_commands"] = manager.commands
            report["blocked_real_systemd_probes"] = len(guard_log.read_text().splitlines()) if guard_log.exists() else 0
        except Exception as error:
            report["error"] = f"{type(error).__name__}: {error}".replace(fixture.state.account_token, "[fixture-token]")
        finally:
            report["checks"] = fixture.checks
            report["cleanup_errors"] = fixture.cleanup()
            if manager is not None:
                manager.close()
            if report["cleanup_errors"]:
                report["success"] = False
            report["cleanup_confirmed"] = all(process.poll() is not None for process in fixture.processes)
    report["duration_seconds"] = round(time.monotonic() - started, 3)
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("007 upgrade requires Linux")
    os.umask(0o077)
    report = run_fixture(args.binary.resolve(strict=True))
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    print(json.dumps({"success": report["success"], "checks": len(report["checks"]),
                      "error": report.get("error"), "cleanup_confirmed": report["cleanup_confirmed"]}, ensure_ascii=False))
    raise SystemExit(not report["success"])


if __name__ == "__main__":
    main()
