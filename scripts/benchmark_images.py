#!/usr/bin/env python3
"""Measure the real ssmcp image path using a bounded, responsive Kitty sink.

The sink records bytes and placement commands, not terminal rendering speed.
Replay the captured synthetic PNGs in a real terminal for a separate pixel gate.
Optional input images and their captures remain private in the chosen directory.
"""
from __future__ import annotations

import argparse
import base64
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import zlib

from benchmark_runtime import (Handler, ServerState, ThreadingHTTPServer,
    binary_identity, launch, owned_tree, same_process, save, signal_owned, wait_for)


PNG_LIMIT = 400 * 1024
TRANSFER_LIMIT = 6 * 1024 * 1024


def chunk(name, data):
    return struct.pack("!I", len(data)) + name + data + struct.pack("!I", zlib.crc32(name + data) & 0xffffffff)


def synthetic_png(width, height, alpha):
    channels = 4 if alpha else 3
    palette = [(248, 249, 250), (60, 64, 67), (66, 133, 244), (234, 67, 53), (52, 168, 83)]
    data = bytearray()
    for y in range(height):
        row = bytearray(b"\0")
        for x in range(0, width, 16):
            seed = ((x // 16 + 3) * 2654435761 + (y // 24 + 1) * 2246822519) & 0xffffffff
            color = palette[(seed >> ((y % 24) % 13)) % len(palette)] if y % 24 < 17 and x % 160 < 128 else palette[0]
            pixel = bytes((*color, 255 if (x + y) % 4 else 224)) if alpha else bytes(color)
            row.extend(pixel * min(16, width - x))
        assert len(row) == width * channels + 1
        data.extend(row)
    header = struct.pack("!IIBBBBB", width, height, 8, 6 if alpha else 2, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", header) + chunk(b"IDAT", zlib.compress(data, 6)) + chunk(b"IEND", b"")


class KittySink:
    def __init__(self, master, out):
        self.master, self.out = master, out
        self.buffer = bytearray()
        self.transfer = None
        self.uploads, self.errors = [], []
        self.queries = 0
        self.cells = 0
        self.wire_bytes = 0
        self.ready = threading.Event()
        self.changed = threading.Condition()
        self.thread = threading.Thread(target=self.run, daemon=True)

    def run(self):
        try:
            with (self.out / "client-prefix.log").open("wb") as log:
                while True:
                    data = os.read(self.master, 65536)
                    if not data:
                        break
                    self.wire_bytes += len(data)
                    if log.tell() < 128 * 1024:
                        log.write(data[:128 * 1024 - log.tell()]); log.flush()
                    self.buffer.extend(data)
                    self.parse()
        except OSError:
            pass
        except Exception as error:
            self.errors.append(type(error).__name__ + ": " + str(error))
        finally:
            with self.changed:
                self.changed.notify_all()

    def parse(self):
        while self.buffer:
            start = self.buffer.find(b"\x1b")
            if start < 0:
                self.buffer.clear(); return
            if start:
                del self.buffer[:start]
            if len(self.buffer) < 3:
                return
            if self.buffer.startswith(b"\x1b_G"):
                end = self.buffer.find(b"\x1b\\", 3)
                if end < 0:
                    if len(self.buffer) > 65536:
                        raise RuntimeError("oversized Kitty protocol chunk")
                    return
                packet = bytes(self.buffer[3:end])
                del self.buffer[:end + 2]
                self.graphics(packet)
            elif self.buffer.startswith(b"\x1b["):
                end = next((i for i in range(2, len(self.buffer)) if 0x40 <= self.buffer[i] <= 0x7e), None)
                if end is None:
                    if len(self.buffer) > 4096:
                        raise RuntimeError("oversized CSI")
                    return
                sequence = bytes(self.buffer[2:end + 1])
                del self.buffer[:end + 1]
                if sequence == b"16t":
                    os.write(self.master, b"\x1b[6;20;10t")
                    self.cells += 1
                elif sequence == b"c":
                    os.write(self.master, b"\x1b[?1;2c")
                if self.queries and self.cells:
                    self.ready.set()
            elif self.buffer.startswith(b"\x1b]"):
                ends = [(i, n) for i, n in ((self.buffer.find(b"\x07", 2), 1), (self.buffer.find(b"\x1b\\", 2), 2)) if i >= 0]
                if not ends:
                    if len(self.buffer) > 65536:
                        raise RuntimeError("oversized OSC")
                    return
                end, size = min(ends)
                del self.buffer[:end + size]
            else:
                del self.buffer[:2]

    def graphics(self, packet):
        control, _, payload = packet.partition(b";")
        params = dict(part.split(b"=", 1) for part in control.split(b",") if b"=" in part)
        action = params.get(b"a")
        if action == b"q":
            identifier = params[b"i"]
            os.write(self.master, b"\x1b_Gi=" + identifier + b";OK\x1b\\")
            self.queries += 1
            if self.cells:
                self.ready.set()
            return
        if action == b"p":
            identifier = int(params[b"i"])
            with self.changed:
                for upload in reversed(self.uploads):
                    if upload["id"] == identifier and "placed_at" not in upload:
                        upload["placed_at"] = time.monotonic()
                        upload["placement"] = {k.decode(): v.decode() for k, v in params.items()}
                        self.changed.notify_all()
                        break
            return
        if action == b"t":
            assert self.transfer is None, "interleaved Kitty transfer"
            assert params.get(b"f") == b"100" and params.get(b"t") == b"d"
            self.transfer = {"id": int(params[b"i"]), "started_at": time.monotonic(), "encoded": bytearray(), "chunks": 0}
        elif action is not None or self.transfer is None:
            return
        transfer = self.transfer
        if len(transfer["encoded"]) + len(payload) > TRANSFER_LIMIT:
            raise RuntimeError("image transfer exceeded fixed sink capacity")
        transfer["encoded"].extend(payload)
        transfer["chunks"] += 1
        if params.get(b"m", b"0") == b"1":
            return
        transfer["completed_at"] = time.monotonic()
        encoded = transfer.pop("encoded")
        png = base64.b64decode(encoded, validate=True)
        assert png.startswith(b"\x89PNG\r\n\x1a\n") and png[12:16] == b"IHDR"
        width, height = struct.unpack("!II", png[16:24])
        transfer.update({"width": width, "height": height, "png_bytes": len(png), "base64_bytes": len(encoded), "sha256": hashlib.sha256(png).hexdigest()})
        with self.changed:
            if len(self.uploads) >= 128:
                raise RuntimeError("sink image-count budget exceeded")
            name = f"transfer-{len(self.uploads):03}.png"
            (self.out / name).write_bytes(png)
            transfer["file"] = name
            self.uploads.append(transfer)
            self.transfer = None
            self.changed.notify_all()

    def wait_image(self, previous, dimensions, timeout=20):
        deadline = time.monotonic() + timeout
        with self.changed:
            while True:
                if self.errors:
                    raise RuntimeError(str(self.errors))
                found = next((r for r in self.uploads[previous:] if (r["width"], r["height"]) == dimensions and "placed_at" in r), None)
                if found:
                    return dict(found)
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise RuntimeError(f"no placed image {dimensions}; uploads={[(r['width'],r['height']) for r in self.uploads[previous:]]}")
                self.changed.wait(remaining)


def cpu_delta(before, after):
    hz = os.sysconf("SC_CLK_TCK")
    # Children that begin and exit wholly between samples are not inferred.
    return sum(max(0, item["cpu_ticks"] - before[pid]["cpu_ticks"]) for pid, item in after.items() if pid in before and item["start_ticks"] == before[pid]["start_ticks"]) / hz


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--input", type=Path, action="append", default=[])
    parser.add_argument("--client-cpu", type=int)
    parser.add_argument("--harness-cpu", type=int)
    args = parser.parse_args()
    if sys.platform != "linux" or not 1 <= args.repeats <= 10 or len(args.input) > 4:
        parser.error("Linux, 1..10 repeats and at most 4 private inputs are required")
    available_cpus = os.sched_getaffinity(0)
    if any(cpu is not None and cpu not in available_cpus for cpu in (args.client_cpu, args.harness_cpu)):
        parser.error("requested CPU is outside the current permitted affinity")
    if args.harness_cpu is not None:
        os.sched_setaffinity(0, {args.harness_cpu})
    binary, out = args.binary.resolve(strict=True), args.out.resolve()
    out.mkdir(parents=True, exist_ok=True)
    if (out / "report.json").exists():
        parser.error("use a fresh output directory")
    provenance = binary_identity(binary)
    report = {"schema": "ssmcp.image-pipeline-benchmark.v1", "ok": False, "complete": False,
        "binary_file": provenance, "source_commit": None,
        "scope": "actual client native image read through completed Kitty transfer and placement command; terminal pixel rendering excluded",
        "cpu_scope": "observed client and owned persistent helpers; short-lived child CPU may be missed",
        "affinity": {"available": sorted(available_cpus), "harness": sorted(os.sched_getaffinity(0)), "client": args.client_cpu},
        "fixture_source_sha256": {name: hashlib.sha256((Path(__file__).parent / name).read_bytes()).hexdigest() for name in (Path(__file__).name, "benchmark_runtime.py", "benchmark_client_memory.py", "e2e_account_relay.py")},
        "measurements": [], "inputs": []}
    save(out / "report.json", report)
    state = ServerState()
    process = sink = server = master = slave = None
    owned = {}
    with tempfile.TemporaryDirectory(prefix="ssmcp-image-benchmark-") as temporary:
        root = Path(temporary)
        for name in ("auth", "workspace", "empty-path", "runtime", "tmp", "config", "cache"):
            (root / name).mkdir(mode=0o700)
        for width, height, alpha in ((2560, 1440, False), (1440, 2560, True), (3840, 2160, True)):
            png = synthetic_png(width, height, alpha)
            assert len(png) <= PNG_LIMIT, f"synthetic source exceeds MCP image limit: {len(png)}"
            name = f"synthetic-{width}x{height}.png"
            (root / "workspace" / name).write_bytes(png)
            (out / name).write_bytes(png)
            report["inputs"].append({"name": name, "private": False, "width": width, "height": height, "bytes": len(png), "sha256": hashlib.sha256(png).hexdigest()})
        for index, path in enumerate(args.input):
            png = path.read_bytes()
            assert png.startswith(b"\x89PNG\r\n\x1a\n") and len(png) <= PNG_LIMIT
            width, height = struct.unpack("!II", png[16:24])
            name = f"private-source-{index + 1}.png"
            (root / "workspace" / name).write_bytes(png)
            report["inputs"].append({"name": name, "private": True, "width": width, "height": height, "bytes": len(png), "sha256": hashlib.sha256(png).hexdigest()})
        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        server.daemon_threads = True
        server.state = state
        state.origin = f"http://127.0.0.1:{server.server_port}"
        server_thread = threading.Thread(target=server.serve_forever, daemon=True)
        server_thread.start()
        credential = root / "auth" / (hashlib.sha256(state.origin.encode()).hexdigest() + ".json")
        save(credential, {"server": state.origin, "token": state.token, "principal": "benchmark@github", "subject": "424242", "username": "benchmark", "expires_at": int(time.time()) + 3600})
        credential.chmod(0o600)
        config = root / "client.toml"
        config.write_text("[ui]\ndashboard = true\ncolor = true\n")
        env = {k: v for k, v in os.environ.items() if not k.startswith("SSMCP_") and k not in ("NO_COLOR", "TMUX", "STY")}
        env.update({"SSMCP_AUTH_HOME": str(root / "auth"), "SSMCP_CLIENT_CONFIG": str(config), "SSMCP_UI_CONFIG": str(config),
            "XDG_CONFIG_HOME": str(root / "config"), "XDG_CACHE_HOME": str(root / "cache"), "XDG_RUNTIME_DIR": str(root / "runtime"),
            "TMPDIR": str(root / "tmp"), "PATH": str(root / "empty-path"), "TERM": "xterm-kitty", "SHELL": "/bin/sh", "ENV": "/dev/null"})
        try:
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 42, 140, 0, 0))
            sink = KittySink(master, out)
            sink.thread.start()
            executable = [str(binary)] if args.client_cpu is None else ["/usr/bin/taskset", "-c", str(args.client_cpu), str(binary)]
            command = [sys.executable, str(Path(__file__).resolve()), "--_launch", *executable, "--server", state.origin,
                "--no-browser", "--no-setup", "--no-watchdog", "--shell", "/bin/sh", "--name", "image-benchmark"]
            process = subprocess.Popen(command, cwd=root / "workspace", env=env, start_new_session=True, stdin=slave, stdout=slave, stderr=slave)
            os.close(slave); slave = None
            def connected():
                if process.poll() is not None:
                    raise RuntimeError(f"client exited ({process.returncode}); see client-prefix.log")
                return state.current()
            peer = wait_for(connected, "image fixture connection")
            assert sink.ready.wait(5), "Kitty/cell probe did not complete"
            time.sleep(0.1)
            for repeat in range(args.repeats):
                for source in report["inputs"]:
                    previous = len(sink.uploads)
                    before = owned_tree(process.pid)
                    owned.update({pid: info["start_ticks"] for pid, info in before.items()})
                    os.write(master, b"\x1b[1;5F")
                    started = time.monotonic()
                    fixture_cpu = time.process_time()
                    result = peer.operation({"op": "call_tool", "name": "ssmcp.fs.read_image", "arguments": {"path": source["name"]}})
                    returned = time.monotonic()
                    del result
                    upload = sink.wait_image(previous, (source["width"], source["height"]))
                    after = owned_tree(process.pid)
                    owned.update({pid: info["start_ticks"] for pid, info in after.items()})
                    row = {"input": source["name"], "private": source["private"], "repeat": repeat + 1,
                        "native_call_seconds": returned - started,
                        "first_transfer_seconds": upload["started_at"] - started,
                        "transfer_complete_seconds": upload["completed_at"] - started,
                        "placement_command_seconds": upload["placed_at"] - started,
                        "observed_cpu_seconds": cpu_delta(before, after),
                        "fixture_cpu_seconds": time.process_time() - fixture_cpu,
                        "source_bytes": source["bytes"], "png_bytes": upload["png_bytes"], "base64_bytes": upload["base64_bytes"],
                        "payload_identical": upload["sha256"] == source["sha256"], "upload": upload}
                    report["measurements"].append(row)
                    save(out / "report.json", report)
                    print(json.dumps({k: row[k] for k in ("input", "repeat", "native_call_seconds", "transfer_complete_seconds", "png_bytes", "observed_cpu_seconds")}), flush=True)
                    time.sleep(0.2)
            assert not state.errors and not sink.errors, str(state.errors + sink.errors)
            report["ok"] = True
        except Exception as error:
            report["error"] = type(error).__name__ + ": " + str(error)
        finally:
            if process is not None:
                owned.update({pid: info["start_ticks"] for pid, info in owned_tree(process.pid).items()})
                if process.poll() is None:
                    process.send_signal(signal.SIGINT)
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        report["ok"] = False; report["shutdown_timeout"] = True
                        process.kill(); process.wait(timeout=5)
                report["client_exit_code"] = process.returncode
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline and any(same_process(pid, start) for pid, start in owned.items() if not process or pid != process.pid):
                time.sleep(0.02)
            remaining = {pid: start for pid, start in owned.items() if (not process or pid != process.pid) and same_process(pid, start)}
            report["owned_pids_remaining_before_harness_cleanup"] = list(remaining)
            if remaining:
                report["ok"] = False
                for pid, start in remaining.items():
                    signal_owned(pid, start, signal.SIGKILL)
            with state.lock:
                for peer in state.peers:
                    peer.close()
            if master is not None:
                os.close(master)
                sink.thread.join(timeout=2)
            if slave is not None:
                os.close(slave)
            server.shutdown(); server.server_close(); server_thread.join(timeout=2)
    report["binary_unchanged"] = binary_identity(binary) == provenance
    report["fixture_source_unchanged"] = all(hashlib.sha256((Path(__file__).parent / name).read_bytes()).hexdigest() == digest for name, digest in report["fixture_source_sha256"].items())
    report["ok"] = report["ok"] and report["binary_unchanged"] and report["fixture_source_unchanged"]
    report["complete"] = True
    report["tty_output_bytes"] = sink.wire_bytes if sink else 0
    save(out / "report.json", report)
    print(json.dumps({"ok": report["ok"], "measurements": len(report["measurements"]), "error": report.get("error"), "report": str(out / "report.json")}), flush=True)
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--_launch":
        launch()
    else:
        raise SystemExit(main())
