#!/usr/bin/env python3
"""Measure a real Linux ssmcp client against a private HTTP/WebSocket peer.

No production account, relay, model, SSH transport, or installed configuration is
used. The peer drives the public wire protocol and native tools. This measures
the client, its real PTY and owned helpers, not a production broker's CPU or TLS.
Raw samples and checkpoints are saved during execution, including failed runs.
"""
from __future__ import annotations

import argparse
import base64
import fcntl
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pty
import queue
import secrets
import signal
import socket
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time

from benchmark_client_memory import binary_identity, memory
from e2e_account_relay import WebSocket


def save(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
    temporary.replace(path)


def process_info(pid):
    path = Path("/proc") / str(pid)
    values = (path / "stat").read_text().rsplit(")", 1)[1].split()
    return {"pid": pid, "start_ticks": int(values[19]), "state": values[0],
            "cpu_ticks": int(values[11]) + int(values[12]),
            "fds": len(list((path / "fd").iterdir())),
            "threads": int(values[17]), **memory(pid)}


def owned_tree(pid):
    """Children may be forked by any Tokio worker, not just the main thread."""
    found, pending = {}, [pid]
    while pending:
        current = pending.pop()
        if current in found:
            continue
        try:
            found[current] = process_info(current)
            children = set()
            for task in (Path("/proc") / str(current) / "task").iterdir():
                try:
                    children.update(int(x) for x in (task / "children").read_text().split())
                except (FileNotFoundError, ProcessLookupError):
                    pass
            pending.extend(children)
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
    return found


def same_process(pid, start):
    try:
        values = (Path("/proc") / str(pid) / "stat").read_text().rsplit(")", 1)[1].split()
        return int(values[19]) == start
    except (FileNotFoundError, ProcessLookupError):
        return False


def signal_owned(pid, start, signum):
    if not same_process(pid, start):
        return
    try:
        fd = os.pidfd_open(pid)
        try:
            if same_process(pid, start):
                signal.pidfd_send_signal(fd, signum)
        finally:
            os.close(fd)
    except ProcessLookupError:
        pass


def wait_for(predicate, description, seconds=10):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.02)
    raise RuntimeError("timed out: " + description)


class Peer(WebSocket):
    def operation(self, operation):
        with self.state.lock:
            self.sequence += 1
            identifier = self.sequence
            result = queue.Queue(maxsize=1)
            self.pending[identifier] = result
        self.send({"type": "call", "id": identifier, "operation": operation})
        try:
            reply = result.get(timeout=15)
        finally:
            with self.state.lock:
                self.pending.pop(identifier, None)
        if "Err" in reply:
            raise RuntimeError("native operation failed: " + str(reply["Err"]))
        value = reply["Ok"]
        if value.get("isError"):
            raise RuntimeError("native operation reported an error")
        return value

    def call(self, name, arguments=None):
        value = self.operation({"op": "call_tool", "name": name, "arguments": arguments or {}})
        return value.get("structuredContent", value)

    def serve(self):
        try:
            self.hello = self.read()
            assert self.hello["type"] == "hello" and self.hello["mode"] == "shell"
            assert self.hello["version"] in (4, 5, 6, 7)
            # This fixture has no account-management workload. Logged-in clients
            # retain their supported legacy Ready behavior without advertised ACL
            # mutation or attribution extensions. Those are tested by real broker tests.
            self.send({"type": "ready", "url": self.state.origin + "/mcp",
                       "client_id": self.id, "login_code": "", "login_expires_in": 0})
            with self.state.lock:
                self.state.peers.append(self)
            while True:
                frame = self.read()
                if frame["type"] == "steering":
                    assert frame["command"] == {"op": "snapshot"}
                    self.send({"type": "steering_reply", "id": frame["id"], "result": {"Ok": {
                        "schema_version": 1, "client_id": self.id, "revision": 0,
                        "agents": [], "messages": []}}})
                elif frame["type"] == "reply":
                    with self.state.lock:
                        result = self.pending.get(frame["id"])
                    if result is not None:
                        result.put_nowait(frame["result"])
                else:
                    raise RuntimeError("unexpected client frame: " + frame["type"])
        except (EOFError, BrokenPipeError, ConnectionResetError, socket.timeout, OSError):
            pass
        except Exception as error:
            self.state.errors.append(type(error).__name__ + ": " + str(error))
        finally:
            self.closed.set()
            with self.state.lock:
                for result in self.pending.values():
                    if result.empty():
                        result.put_nowait({"Err": "fixture connection closed"})


class ServerState:
    def __init__(self):
        self.lock = threading.RLock()
        self.token = "cl_" + secrets.token_hex(32)
        self.peers, self.errors = [], []
        self.origin = ""

    def current(self):
        with self.lock:
            return next((peer for peer in reversed(self.peers) if not peer.closed.is_set()), None)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def respond(self, code, value):
        data = json.dumps(value).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(data)
        self.close_connection = True

    def do_GET(self):
        state = self.server.state
        if self.headers.get("Authorization") != "Bearer " + state.token:
            return self.respond(401, {"error": "invalid_token"})
        if self.path == "/api/client/whoami":
            return self.respond(200, {"provider": "github", "subject": "424242",
                "login": "benchmark", "principal": "benchmark@github", "username": "benchmark"})
        if self.path != "/api/client/connect":
            return self.respond(404, {"error": "not_found"})
        key = self.headers.get("Sec-WebSocket-Key", "")
        accept = base64.b64encode(hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()).decode()
        self.send_response(101)
        self.send_header("Upgrade", "websocket")
        self.send_header("Connection", "Upgrade")
        self.send_header("Sec-WebSocket-Accept", accept)
        self.end_headers()
        self.close_connection = True
        self.connection.settimeout(None)
        Peer(self, state.token, state).serve()


class Sampler:
    def __init__(self, process, out, interval):
        self.process, self.out, self.interval = process, out, interval
        self.phase, self.rows, self.owned = "startup", [], {}
        self.previous_cpu = {}
        self.stop = threading.Event()
        self.started = time.monotonic()
        self.thread = threading.Thread(target=self.run, daemon=True)

    def run(self):
        with (self.out / "samples.jsonl").open("w") as stream:
            while not self.stop.is_set():
                tree = owned_tree(self.process.pid)
                for pid, row in tree.items():
                    self.owned[pid] = row["start_ticks"]
                main = tree.get(self.process.pid)
                if main is not None:
                    observed_cpu = 0
                    for pid, process in tree.items():
                        identity = (pid, process["start_ticks"])
                        previous = self.previous_cpu.get(identity, process["cpu_ticks"])
                        observed_cpu += max(0, process["cpu_ticks"] - previous)
                        self.previous_cpu[identity] = process["cpu_ticks"]
                    row = {"seconds": round(time.monotonic() - self.started, 4), "phase": self.phase,
                           "client": main, "owned_processes": len(tree),
                           "owned_rss_swap_kib": sum(x["rss_swap_kib"] for x in tree.values()),
                           "owned_pss_kib": sum(x["pss_kib"] or 0 for x in tree.values()),
                           "owned_fds": sum(x["fds"] for x in tree.values()),
                           "sampled_owned_cpu_ticks_delta": observed_cpu}
                    self.rows.append(row)
                    stream.write(json.dumps(row) + "\n")
                    stream.flush()
                self.stop.wait(self.interval)

    def finish(self):
        self.stop.set()
        self.thread.join(timeout=5)
        assert not self.thread.is_alive(), "sampler did not stop"


def scan(peer):
    cursor, seen, names = None, set(), set()
    started = time.monotonic()
    while True:
        result = peer.operation({"op": "list_tools", **({"cursor": cursor} if cursor else {})})
        for tool in result["tools"]:
            assert tool["name"] not in names, "duplicate catalog tool"
            names.add(tool["name"])
        cursor = result.get("nextCursor")
        if not cursor:
            break
        assert cursor not in seen and len(seen) < 32, "invalid catalog cursor"
        seen.add(cursor)
    return {"seconds": time.monotonic() - started, "tools": len(names), "pages": len(seen) + 1}


def cycle(peer, number, output_bytes):
    started = time.monotonic()
    expected = f"PERF_{number:05}_" + "x" * output_bytes + "\n"
    fragments = [expected[offset:offset + 4096] for offset in range(0, len(expected), 4096)]
    result = peer.call("ssmcp.exec.start", {"command": ["/bin/sh", "-c", "printf '%s' \"$@\"", "benchmark", *fragments], "timeout_ms": 5000})
    job = result["job_id"]
    if result["running"]:
        result = peer.call("ssmcp.exec.wait", {"job_id": job, "wait_ms": 5000})
    assert not result["running"] and result["exit_code"] == 0
    read = peer.call("ssmcp.exec.read", {"job_id": job, "max_bytes": 32768})
    assert base64.b64decode(read["stdout_base64"], validate=True) == expected.encode()
    assert read["stdout_eof"] and not read["stdout_truncated"]
    peer.call("ssmcp.exec.forget", {"job_id": job})
    terminal = peer.call("terminal.open", {"name": f"benchmark-{number}"})
    identifier = terminal["terminal_id"]
    status = peer.call("terminal.status", {"terminal_id": identifier})
    assert status["running"] and isinstance(status["pid"], int)
    peer.call("terminal.close", {"terminal_id": identifier})
    assert not (Path("/proc") / str(status["pid"])).exists(), "closed terminal process remains"
    return {"cycle": number, "seconds": round(time.monotonic() - started, 6),
            "verified_output_bytes": len(expected.encode())}


def summaries(rows):
    phases = {}
    hz = os.sysconf("SC_CLK_TCK")
    for name in dict.fromkeys(row["phase"] for row in rows):
        values = [row for row in rows if row["phase"] == name]
        result = {"samples": len(values)}
        for key in ("rss_swap_kib", "pss_kib", "fds", "threads"):
            numbers = [row["client"][key] for row in values]
            result["client_" + key] = {"first": numbers[0], "last": numbers[-1], "max": max(numbers), "median": statistics.median(numbers)}
        for key in ("owned_processes", "owned_rss_swap_kib", "owned_pss_kib", "owned_fds"):
            numbers = [row[key] for row in values]
            result[key] = {"first": numbers[0], "last": numbers[-1], "max": max(numbers), "median": statistics.median(numbers)}
        span = values[-1]["seconds"] - values[0]["seconds"]
        cpu = (values[-1]["client"]["cpu_ticks"] - values[0]["client"]["cpu_ticks"]) / hz
        result["measured_seconds"] = span
        result["client_cpu_seconds"] = cpu
        result["client_cpu_percent_one_core"] = 100 * cpu / span if span > 0 else None
        sampled_cpu = sum(row["sampled_owned_cpu_ticks_delta"] for row in values[1:]) / hz
        result["sampled_owned_cpu_seconds_lower_bound"] = sampled_cpu
        result["sampled_owned_cpu_percent_one_core_lower_bound"] = 100 * sampled_cpu / span if span > 0 else None
        if name == "soak" and len(values) > 2:
            # Report a finite-window trend; this is not a proof of leak freedom.
            warm = values[len(values) // 5:]
            xs = [row["seconds"] / 60 for row in warm]
            ys = [row["client"]["pss_kib"] for row in warm]
            mx, my = statistics.mean(xs), statistics.mean(ys)
            denominator = sum((x - mx) ** 2 for x in xs)
            result["post_warmup_pss_kib_per_minute"] = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / denominator if denominator else None
            result["trend_warmup_fraction"] = 0.2
        phases[name] = result
    return phases


def launch():
    # Runs after exec in a fresh interpreter, avoiding a Python preexec hook in
    # the multi-threaded benchmark process. Popen has already made a new session.
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    os.execv(sys.argv[2], sys.argv[2:])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--idle-seconds", type=float, default=10)
    parser.add_argument("--soak-seconds", type=float, default=60)
    parser.add_argument("--cycles", type=int, default=20)
    parser.add_argument("--cycle-interval", type=float, default=1)
    parser.add_argument("--reconnect-every", type=int, default=10)
    parser.add_argument("--output-bytes", type=int, default=8192)
    parser.add_argument("--sample-ms", type=int, default=1000)
    parser.add_argument("--no-tty", action="store_true")
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("Linux /proc and pidfd are required")
    if not (0.2 <= args.idle_seconds <= 300 and 0 <= args.soak_seconds <= 3600 and 1 <= args.cycles <= 2000 and 0 <= args.cycle_interval <= 60 and 0 <= args.reconnect_every <= 2000 and 0 <= args.output_bytes <= 30000 and 50 <= args.sample_ms <= 5000):
        parser.error("invalid bounded benchmark settings")
    binary, out = args.binary.resolve(strict=True), args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    if (out / "report.json").exists():
        parser.error("use a fresh output directory; previous evidence is never overwritten")
    provenance = binary_identity(binary)
    report = {"schema": "ssmcp.runtime-benchmark.v1", "complete": False, "ok": False,
              "binary": str(binary), "binary_file": provenance,
              "source_commit": None, "fixture": "private loopback HTTP/WebSocket with synthetic saved login",
              "scope": "real client/PTY/owned-helper resources; production broker and TLS excluded",
              "cpu_scope": "client process CPU; owned-child CPU is not inferred from sparse samples",
              "settings": {k: v for k, v in vars(args).items() if k not in ("binary", "out")},
              "cycles": [], "catalog_scans": [], "reconnections": 0,
              "fixture_source_sha256": {name: hashlib.sha256((Path(__file__).parent / name).read_bytes()).hexdigest() for name in (Path(__file__).name, "benchmark_client_memory.py", "e2e_account_relay.py")}}
    save(out / "report.json", report)
    process = sampler = server = master = drain = None
    state = ServerState()
    output_bytes = 0
    with tempfile.TemporaryDirectory(prefix="ssmcp-runtime-benchmark-") as temporary:
        root = Path(temporary)
        for name in ("auth", "workspace", "empty-path", "runtime", "tmp", "config", "cache"):
            (root / name).mkdir(mode=0o700)
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        server.state = state
        state.origin = f"http://127.0.0.1:{server.server_port}"
        server_thread = threading.Thread(target=server.serve_forever, daemon=True)
        server_thread.start()
        credential = root / "auth" / (hashlib.sha256(state.origin.encode()).hexdigest() + ".json")
        save(credential, {"server": state.origin, "token": state.token,
             "principal": "benchmark@github", "subject": "424242", "username": "benchmark", "expires_at": int(time.time()) + 7200})
        credential.chmod(0o600)
        config = root / "client.toml"
        config.write_text("[ui]\ndashboard = true\ncolor = false\n")
        env = {k: v for k, v in os.environ.items() if not k.startswith("SSMCP_")}
        env.update({"SSMCP_AUTH_HOME": str(root / "auth"), "SSMCP_CLIENT_CONFIG": str(config),
                    "SSMCP_UI_CONFIG": str(config), "XDG_CONFIG_HOME": str(root / "config"),
                    "XDG_CACHE_HOME": str(root / "cache"), "XDG_RUNTIME_DIR": str(root / "runtime"),
                    "TMPDIR": str(root / "tmp"), "PATH": str(root / "empty-path"),
                    "TERM": "xterm-256color", "SHELL": "/bin/sh", "ENV": "/dev/null", "NO_COLOR": "1"})
        command = [str(binary), "--server", state.origin, "--no-browser", "--no-setup",
                   "--shell", "/bin/sh", "--name", "runtime-benchmark", "--reconnect-delay", "1"]
        slave = None
        log = (out / "client.log").open("wb")
        try:
            if not args.no_tty:
                master, slave = pty.openpty()
                fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 38, 120, 0, 0))
                def consume():
                    nonlocal output_bytes
                    try:
                        while True:
                            data = os.read(master, 65536)
                            if not data:
                                break
                            output_bytes += len(data)
                            # Retain only a bounded diagnostic prefix, never a growing
                            # capture of every repaint during an hour-long soak.
                            if log.tell() < 128 * 1024:
                                log.write(data[:128 * 1024 - log.tell()])
                                log.flush()
                    except OSError:
                        pass
                drain = threading.Thread(target=consume, daemon=True)
                drain.start()
                command = [sys.executable, str(Path(__file__).resolve()), "--_launch", *command]
            process = subprocess.Popen(command, cwd=root / "workspace", env=env, start_new_session=True,
                stdin=slave if slave is not None else subprocess.DEVNULL,
                stdout=slave if slave is not None else log, stderr=slave if slave is not None else log)
            if slave is not None:
                os.close(slave)
                slave = None
            # Do not count the temporary Python TTY launcher as ssmcp.
            def executed():
                if process.poll() is not None:
                    raise RuntimeError(f"client exited during startup ({process.returncode}); see client.log")
                return Path(f"/proc/{process.pid}/exe").resolve() == binary
            wait_for(executed, "client exec")
            sampler = Sampler(process, out, args.sample_ms / 1000)
            sampler.thread.start()
            peer = wait_for(state.current, "private client connection")
            report["wire_version"] = peer.hello["version"]
            report["catalog_scans"].append(scan(peer))
            sampler.phase = "initial_idle"
            time.sleep(args.idle_seconds)
            sampler.phase = "soak"
            started = time.monotonic()
            number = 0
            with (out / "cycles.jsonl").open("w") as events:
                while number < args.cycles or time.monotonic() - started < args.soak_seconds:
                    if number >= 10000:
                        raise RuntimeError("cycle budget reached")
                    if process.poll() is not None:
                        raise RuntimeError("client exited during workload")
                    number += 1
                    row = cycle(peer, number, args.output_bytes)
                    events.write(json.dumps(row) + "\n"); events.flush()
                    report["cycles"].append(row)
                    if args.reconnect_every and number % args.reconnect_every == 0:
                        old = peer
                        old.close()
                        peer = wait_for(lambda: (p if (p := state.current()) is not None and p is not old else None), "client reconnect")
                        report["reconnections"] += 1
                        report["catalog_scans"].append(scan(peer))
                    if number % 20 == 0:
                        save(out / "report.json", report)
                        print(f"verified cycles={number} reconnects={report['reconnections']}", flush=True)
                    time.sleep(args.cycle_interval)
            sampler.phase = "final_idle"
            time.sleep(args.idle_seconds)
            report["catalog_scans"].append(scan(peer))
            assert not state.errors, str(state.errors)
            report["ok"] = True
        except Exception as error:
            report["error"] = type(error).__name__ + ": " + str(error)
        finally:
            if sampler:
                sampler.phase = "shutdown"
            if process is not None:
                if sampler:
                    sampler.owned.update({pid: row["start_ticks"] for pid, row in owned_tree(process.pid).items()})
                if process.poll() is None:
                    process.send_signal(signal.SIGINT)
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        report["ok"] = False
                        report["shutdown_timeout"] = True
                        process.kill(); process.wait(timeout=5)
                report["client_exit_code"] = process.returncode
            if sampler:
                sampler.finish()
                deadline = time.monotonic() + 2
                remaining = {}
                while time.monotonic() < deadline:
                    remaining = {pid: start for pid, start in sampler.owned.items() if pid != process.pid and same_process(pid, start)}
                    if not remaining:
                        break
                    time.sleep(0.02)
                report["owned_pids_remaining_before_harness_cleanup"] = sorted(remaining)
                if remaining:
                    report["ok"] = False
                    for pid, start in remaining.items():
                        signal_owned(pid, start, signal.SIGTERM)
                    time.sleep(0.2)
                    for pid, start in remaining.items():
                        signal_owned(pid, start, signal.SIGKILL)
                report["phases"] = summaries(sampler.rows)
            with state.lock:
                for peer in state.peers:
                    peer.close()
                    try:
                        peer.handler.connection.shutdown(socket.SHUT_RDWR)
                    except OSError:
                        pass
            if master is not None:
                os.close(master)
                if drain:
                    drain.join(timeout=2)
            if slave is not None:
                os.close(slave)
            server.shutdown(); server.server_close(); server_thread.join(timeout=2)
            log.close()
    report["tty_output_bytes"] = output_bytes
    report["binary_unchanged"] = binary_identity(binary) == provenance
    report["fixture_source_unchanged"] = all(hashlib.sha256((Path(__file__).parent / name).read_bytes()).hexdigest() == expected for name, expected in report["fixture_source_sha256"].items())
    report["ok"] = report["ok"] and report["binary_unchanged"] and report["fixture_source_unchanged"]
    report["complete"] = True
    report["elapsed_seconds"] = round(time.monotonic() - sampler.started, 3) if sampler else None
    save(out / "report.json", report)
    print(json.dumps({"ok": report["ok"], "cycles": len(report["cycles"]), "reconnections": report["reconnections"], "report": str(out / "report.json"), "error": report.get("error")}), flush=True)
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--_launch":
        launch()
    else:
        raise SystemExit(main())
