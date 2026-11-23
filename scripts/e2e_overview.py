#!/usr/bin/env python3
"""Owned Linux clients and private IPC exercise the actual overview CLI.

No installed services, accounts, models or user sessions are changed. A separate
synthetic private endpoint drops one receipt to test unknown/no-replay handling.
"""
import argparse
import hashlib
import json
import os
import re
from pathlib import Path
import secrets
import signal
import socket
import struct
import subprocess
import tempfile
import threading
import time


def receive(stream):
    def exact(count):
        result = b""
        while len(result) < count:
            part = stream.recv(count - len(result))
            if not part:
                raise EOFError("private IPC reply closed")
            result += part
        return result
    size = struct.unpack("!I", exact(4))[0]
    if not 0 < size <= 256 * 1024:
        raise ValueError("invalid private frame length")
    return json.loads(exact(size))


def send(stream, value):
    data = json.dumps(value).encode()
    stream.sendall(struct.pack("!I", len(data)) + data)


def ipc(directory, request):
    endpoint = json.loads((directory / "attach.json").read_text())
    with socket.create_connection(("127.0.0.1", endpoint["port"]), timeout=5) as stream:
        send(stream, {"token": endpoint["token"], "request": request})
        value = receive(stream)
    if value.get("error"):
        raise RuntimeError("private endpoint rejected request")
    return value["result"]


def bridge(directory, lease, operation):
    return ipc(directory, {"op": "bridge", "lease": lease, "request": {
        "version": 1, "id": secrets.token_hex(8), "operation": operation}})


def wait_for(test, timeout=20):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        value = test()
        if value:
            return value
        time.sleep(0.1)
    raise AssertionError("bounded overview observation did not become ready")


def private_json(path, value):
    with path.open("x") as stream:
        os.chmod(path, 0o600)
        json.dump(value, stream)


class LostReceipt:
    """Protocol fixture only: no native command is executed by this endpoint."""
    def __init__(self, sessions):
        self.directory = sessions / "live" / secrets.token_hex(16)
        self.directory.mkdir(mode=0o700)
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(8)
        self.listener.settimeout(0.2)
        self.token, self.lease = secrets.token_hex(32), secrets.token_hex(16)
        self.count, self.stopped, self.errors = 0, False, []
        private_json(self.directory / "attach.json", {"format": 1, "port": self.listener.getsockname()[1],
                     "token": self.token, "pid": os.getpid()})
        private_json(self.directory / "overview.json", {"format": 1, "instance": self.directory.name,
                     "pid": os.getpid(), "process_start_ticks": None,
                     "metadata": {"name": "lost-receipt-fixture", "title": None,
                                  "cwd": str(sessions), "hostname": "fixture", "allow": []}})
        self.thread = threading.Thread(target=self.serve, daemon=True)
        self.thread.start()

    def serve(self):
        while not self.stopped:
            try:
                connection, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            try:
                with connection:
                    connection.settimeout(3)
                    outer = receive(connection)
                    assert outer["token"] == self.token
                    value = outer["request"]
                    if value["op"] == "open":
                        assert value["read_only"] is False
                        result = {"lease": self.lease, "read_only": False}
                    elif value["op"] == "detach":
                        result = {"detached": True, "session_stopped": False}
                    else:
                        assert value["lease"] == self.lease
                        request = value["request"]
                        if request["operation"]["op"] == "command":
                            self.count += 1
                            continue  # A command was received; its receipt is lost.
                        assert request["operation"]["op"] == "capabilities"
                        result = {"version": 1, "id": request["id"], "generation": "fixture-1", "body": {
                            "kind": "capabilities", "value": {"application": "ssmcp", "mode": "control",
                            "resources": [], "commands": [{"id": "exec.start", "description": "fixture",
                            "input_schema": {"type": "object"}}], "events": False,
                            "max_frame_bytes": 260000, "max_event_page_bytes": 131072}}}
                    send(connection, {"result": result, "error": None})
            except Exception as error:
                self.errors.append(type(error).__name__)

    def close(self):
        self.stopped = True
        self.listener.close()
        self.thread.join(timeout=3)


def run(binary, report):
    checks, processes, handles = [], [], []
    report.update(scope="two actual Linux local clients plus one synthetic lost-receipt endpoint",
                  installed_services_changed=False, real_credentials_used=False,
                  binary_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(), success=False)

    def check(condition, description):
        if not condition:
            raise AssertionError(description)
        checks.append(description)
        print("PASS " + description, flush=True)

    with tempfile.TemporaryDirectory(prefix="ssmcp-overview-e2e-") as temporary:
        root = Path(temporary)
        home, sessions, workspace = root / "home", root / "sessions", root / "workspace"
        for path in (home, sessions, workspace):
            path.mkdir(mode=0o700)
        environment = {key: value for key, value in os.environ.items() if not key.startswith("SSMCP_")}
        environment.update(HOME=str(home), USERPROFILE=str(home), SSMCP_SESSION_HOME=str(sessions),
                           XDG_CONFIG_HOME=str(home / "config"), XDG_STATE_HOME=str(home / "state"),
                           ENV="/dev/null", BASH_ENV="/dev/null", NO_COLOR="1", TERM="dumb")

        def command(*args, success=True):
            result = subprocess.run([str(binary), "overview", *args], cwd=workspace, env=environment,
                                    capture_output=True, text=True, timeout=20)
            if success and result.returncode:
                raise AssertionError("overview command failed: " + result.stderr[-500:])
            return result

        def overview():
            return json.loads(command("--json").stdout)

        fixture = None
        try:
            empty = overview()
            check(empty["sessions"] == [] and not (sessions / "live").exists(),
                  "read-only empty overview creates no discovery registry")
            for index in range(2):
                log = (root / f"client-{index}.log").open("w")
                handles.append(log)
                processes.append(subprocess.Popen([str(binary), "local", "--input", "none",
                    "--name", "same-label", "--log-format", "jsonl", "--no-watchdog"],
                    cwd=workspace, env=environment, stdout=log, stderr=log, start_new_session=True))

            def connected():
                if any(process.poll() is not None for process in processes):
                    raise AssertionError("owned local client exited before overview acceptance")
                value = overview()
                return value if len(value["sessions"]) == 2 and all(row["state"] == "connected" for row in value["sessions"]) else None
            observed = wait_for(connected, timeout=35)
            targets = [row["target"] for row in observed["sessions"]]
            check(len(set(targets)) == 2 and {row["name"] for row in observed["sessions"]} == {"same-label"},
                  "two real same-label clients retain distinct exact selectors")
            check({row["pid"] for row in observed["sessions"]} == {process.pid for process in processes},
                  "overview reports actual client PIDs rather than manager or model estimates")
            check(all(row["metrics"]["rss_bytes"] > 0 and row["metrics"]["cpu_percent_one_core"] is None
                      for row in observed["sessions"]), "first CPU sample is unknown while Linux RSS is measured")
            watch = subprocess.Popen([str(binary), "overview", "--watch", "--json"], cwd=workspace,
                                     env=environment, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            try:
                try:
                    watch.communicate(timeout=5)
                    raise AssertionError("overview watch exited without an explicit stop")
                except subprocess.TimeoutExpired:
                    watch.send_signal(signal.SIGINT)
                    output, _ = watch.communicate(timeout=5)
                snapshots = [json.loads(line) for line in output.splitlines() if line.strip()]
                check(len(snapshots) >= 2 and all(row["metrics"]["cpu_percent_one_core"] is not None
                      for row in snapshots[1]["sessions"]), "watch measures CPU only after an actual second sample")
            finally:
                if watch.poll() is None:
                    watch.kill()
                    watch.wait(timeout=3)
            for directory in (sessions / "live").iterdir():
                token = json.loads((directory / "attach.json").read_text())["token"]
                check(token not in json.dumps(observed), "overview never serializes an endpoint credential")

            marker = workspace / "one-native-command.txt"
            result = json.loads(command("exec", targets[0], "--", "/bin/sh", "-c", "printf x >> one-native-command.txt").stdout)
            receipt = result["result"]["receipt"]
            native = receipt.get("structuredContent", receipt)
            job = native["job_id"]
            wait_for(lambda: marker.is_file() and marker.read_bytes() == b"x")
            jobs = wait_for(lambda: next((row for row in json.loads(command("jobs", targets[0]).stdout)["jobs"]
                                          if row["job_id"] == job and row["state"] == "completed"), None))
            check(jobs["exit_code"] == 0, "explicit argv executes once and retains its actual native completion")
            other = json.loads(command("jobs", targets[1]).stdout)
            check(all(row["job_id"] != job for row in other["jobs"]), "exact dispatch never sends the job to the other same-label client")
            check("one-native-command" not in json.dumps(other), "job summary omits command arguments and output")
            directory = sessions / "live" / targets[0].split(":", 1)[1]
            lease = ipc(directory, {"op": "open", "read_only": True})["lease"]
            cap = bridge(directory, lease, {"op": "capabilities"})
            denied = bridge(directory, lease, {"op": "command", "generation": cap["generation"], "command": "exec.start",
                "arguments": {"command": ["/bin/sh", "-c", "printf bad >> one-native-command.txt"]}})
            check(denied["body"]["kind"] == "error" and denied["body"]["value"]["code"] == "denied",
                  "read-only private lease rejects native execution before dispatch")
            ipc(directory, {"op": "detach", "lease": lease})
            lease = ipc(directory, {"op": "open", "read_only": False})["lease"]
            stale = bridge(directory, lease, {"op": "command", "generation": "old-generation", "command": "exec.start",
                "arguments": {"command": ["/bin/sh", "-c", "printf bad >> one-native-command.txt"]}})
            check(stale["body"]["kind"] == "error" and stale["body"]["value"]["code"] == "stale_generation",
                  "stale generation cannot reach a reconnected backend")
            ipc(directory, {"op": "detach", "lease": lease})
            check(marker.read_bytes() == b"x", "denied and stale writes leave the original command count unchanged")
            check(command("exec", "*", "--", "/bin/true", success=False).returncode != 0,
                  "broadcast and display-name selectors are unavailable")

            fixture = LostReceipt(sessions)
            lost = command("exec", "live:" + fixture.directory.name, "--", "/bin/true", success=False)
            receipt = json.loads(lost.stdout)
            check(lost.returncode != 0 and receipt["result"]["outcome"] == "unknown",
                  "lost private command receipt is reported as unknown")
            check(fixture.count == 1 and not fixture.errors, "unknown execution is never automatically replayed")
            fixture.close()
            fixture = None
            report["success"] = True
        except Exception as error:
            report["error"] = f"{type(error).__name__}: {error}".replace(str(root), "<fixture>")
            def redact_tail(value):
                value = re.sub(r"https?://\S+", "<fixture-url>", value)
                value = re.sub(r"\b[a-fA-F0-9]{32,}\b", "<fixture-id>", value)
                return value.replace(str(root), "<fixture>")
            report["client_log_tails"] = [redact_tail((root / f"client-{i}.log").read_text(errors="replace")[-2000:])
                                           for i in range(len(processes))]
        finally:
            if fixture:
                fixture.close()
            for process in reversed(processes):
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=8)
                    except subprocess.TimeoutExpired:
                        os.killpg(process.pid, signal.SIGKILL)
                        process.wait(timeout=5)
                        report["success"] = False
                        report["cleanup_error"] = "owned local client needed forced cleanup"
            for handle in handles:
                handle.close()
    report["checks"] = checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--report", type=Path, required=True)
    args = parser.parse_args()
    report = {"schema": "ssmcp.overview.e2e.v1"}
    run(args.binary.resolve(), report)
    args.report.parent.mkdir(parents=True, exist_ok=True)
    args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps({"success": report["success"], "checks": len(report["checks"]), "report": str(args.report)}))
    raise SystemExit(0 if report["success"] else 1)


if __name__ == "__main__":
    main()
