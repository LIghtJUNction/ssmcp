#!/usr/bin/env python3
"""Linux client CPU/RSS benchmark using a private HTTP/WebSocket relay.

Run with --binary target/release/ssmcp. --providers chooses a minimal client,
a synthetic paginated MCP catalog (default), or the installed PATH with lazy bridges.
Bubblewrap isolates network, process, desktop and state paths before any client
starts. The fixture uses no real credentials, desktop input or model calls.
Foreground TUI is measured at 120x38; --no-tty measures redirected output.
--max-footprint-mib gates sampled RSS + swap after discovery, through catalog
scans, verified commands, and subsequent idle. Startup peak is also reported.
A final catalog scan checks stable fixtures and preservation of installed tools.
Phase CPU is sampled as client user+system time (100% is one core).
--draft and --completion-fixtures exercise an unsent editor without sending work.
Raw keyboard and exact conversation draft checks precede both timed idle windows.
Actual phase durations, sampled CPU windows and source/binary hashes are reported.
Completed jobs are retained by default; --forget-jobs reproduces older benchmarks.
The paginated search workload filters pages like code_mode's server-side helper;
the helper itself and native child processes are outside the client RSS metric.
"""
from __future__ import annotations

import argparse
import base64
import fcntl
import hashlib
import importlib.util
import json
import os
import platform
from pathlib import Path
import pty
import queue
import shutil
import signal
import statistics
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import zlib


def save(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value), encoding="utf-8")
    temporary.replace(path)


def directory():
    return Path(os.environ["SSMCP_MEMORY_DIR"])


def send(value):
    print(json.dumps(value, separators=(",", ":")), flush=True)


def native():
    if sys.argv[1:] == ["--version"]:
        print("ssmcp-memory-fixture 1.0")
        return
    config = json.loads((directory() / "config.json").read_text())
    count = config["tools"]
    description = "Generated memory benchmark descriptor. " + "x" * config["description_bytes"]
    for line in sys.stdin:
        request = json.loads(line)
        method, params = request.get("method"), request.get("params", {})
        if method == "initialize":
            result = {"protocolVersion": params["protocolVersion"], "capabilities": {"tools": {}},
                      "serverInfo": {"name": "memory-fixture", "version": "1"}}
        elif method == "tools/list":
            start = int(params.get("cursor", "0"))
            end = min(start + 32, count)
            result = {"tools": [{
                "name": f"memory_probe_{i:05}", "description": description,
                "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}},
                                "additionalProperties": False},
                "annotations": {"readOnlyHint": True}, "_meta": {"fixture_index": i},
            } for i in range(start, end)]}
            if end < count:
                result["nextCursor"] = str(end)
        elif method == "tools/call":
            result = {"content": [], "structuredContent": {"arguments": params["arguments"]}}
        else:
            continue
        send({"jsonrpc": "2.0", "id": request["id"], "result": result})


def relay(root, connection, prepare_draft):
    config = json.loads((root / "config.json").read_text())
    current_phase = "startup"
    phase_started = time.monotonic()

    def phase(name):
        nonlocal current_phase, phase_started
        ended = time.monotonic()
        with (root / "phase-durations.jsonl").open("a") as stream:
            stream.write(json.dumps({"phase": current_phase, "started_monotonic": phase_started,
                                     "ended_monotonic": ended, "seconds": ended - phase_started}) + "\n")
        if current_phase not in ("startup", "discovery"):
            pid = int((root / "client.pid").read_text())
            checkpoint = {"elapsed_seconds": time.monotonic(), "phase": current_phase, **memory(pid)}
            with (root / "checkpoints.jsonl").open("a") as stream:
                stream.write(json.dumps(checkpoint) + "\n")
            if current_phase in ("idle", "after_workload_idle"):
                save(root / f"mappings-{current_phase}.json", memory_details(pid))
        save(root / "phase.json", name)
        current_phase = name
        phase_started = time.monotonic()

    def request(operation):
        # Reuse the exercised account fixture's reader and connection-bound reply
        # queues. Only this workload sends native calls; no model is involved.
        with connection.state.lock:
            connection.sequence += 1
            identifier = connection.sequence
            result = queue.Queue(maxsize=1)
            connection.pending[identifier] = result
        try:
            connection.send({"type": "call", "id": identifier, "operation": operation})
            reply = result.get(timeout=15)
        finally:
            with connection.state.lock:
                connection.pending.pop(identifier, None)
        if "Err" in reply:
            raise RuntimeError(reply["Err"])
        return reply["Ok"]

    def call(name, arguments=None):
        result = request({"op": "call_tool", "name": name, "arguments": arguments or {}})
        if result.get("isError"):
            raise RuntimeError(f"{name} returned an error")
        return result.get("structuredContent", result)

    def scan(gathered=None, observed_names=None):
        cursor, cursors, names = None, set(), set()
        pages = byte_count = matches = 0
        probe = None
        digest = hashlib.sha256()
        while True:
            page = request({"op": "list_tools", **({"cursor": cursor} if cursor else {})})
            pages += 1
            for tool in page["tools"]:
                if gathered is not None:
                    gathered.append(tool)
                if tool["name"] in names:
                    raise RuntimeError("duplicate catalog name")
                names.add(tool["name"])
                if tool.get("_meta", {}).get("fixture_index") == 0:
                    probe = tool["name"]
                encoded = json.dumps(tool, separators=(",", ":"), sort_keys=True).encode()
                byte_count += len(encoded)
                digest.update(encoded)
                matches += "memory" in (tool["name"] + " " + tool.get("description", "")).lower()
            cursor = page.get("nextCursor")
            if not cursor:
                if observed_names is not None:
                    observed_names.update(names)
                return {"tools": len(names), "pages": pages, "compact_json_bytes": byte_count,
                        "search_matches": matches, "sha256": digest.hexdigest(), "probe": probe}
            if cursor in cursors:
                raise RuntimeError("catalog cursor repeated")
            cursors.add(cursor)

    try:
        phase("discovery")
        # Optional providers are lazy. Loading this Python fixture registers a
        # deterministic MCP catalog without starting a real model turn.
        if config["providers"] == "synthetic":
            call("ssmcp.harnesses.load", {"provider": "claude"})
        deadline = time.monotonic() + config["discovery_timeout"]
        while True:
            harnesses = call("ssmcp.harnesses.list")["harnesses"]
            if all(h["status"] != "discovering" for h in harnesses):
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("native discovery timed out")
            time.sleep(0.1)
        if config["providers"] == "synthetic":
            found = next(h for h in harnesses if h["provider"] == "claude")
            if found["status"] != "ready" or found["tools"] < config["tools"]:
                raise RuntimeError("synthetic catalog was not registered")
        phase("prepare_draft")
        prepare_draft(connection, "idle")
        phase("idle")
        time.sleep(config["idle_seconds"])
        phase("catalog_scan_and_search")
        scans, scanned_names = [], []
        for _ in range(config["scans"]):
            names = set()
            scans.append(scan(observed_names=names))
            scanned_names.append(names)
        if config["providers"] == "synthetic":
            if any(s["tools"] < config["tools"] for s in scans):
                raise RuntimeError("synthetic catalog was truncated")
            result = call(scans[0]["probe"], {"text": "call-check"})
            if result.get("arguments") != {"text": "call-check"}:
                raise RuntimeError("native tool dispatch changed arguments")
        phase("commands_and_logs")
        byte_count = 0
        for index in range(config["commands"]):
            expected = f"MEMORY_{index:05} " + "z" * config["command_bytes"] + "\n"
            job = call("ssmcp.exec.start", {"command": ["/bin/sh", "-c", 'printf "%s\\n" "$1"',
                       "memory-fixture", expected[:-1]], "timeout_ms": 5000})["job_id"]
            done = call("ssmcp.exec.wait", {"job_id": job, "wait_ms": 5000})
            if done["running"] or done["exit_code"] != 0:
                raise RuntimeError("fixture command did not finish successfully")
            read = call("ssmcp.exec.read", {"job_id": job, "max_bytes": 32768})
            actual = base64.b64decode(read["stdout_base64"], validate=True)
            if actual != expected.encode() or not read["stdout_eof"] or read["stdout_truncated"]:
                raise RuntimeError("command output was lost or truncated")
            byte_count += len(actual)
            if config.get("forget_jobs", True):
                call("ssmcp.exec.forget", {"job_id": job})
        phase("prepare_after_workload_idle")
        prepare_draft(connection, "after_workload_idle")
        phase("after_workload_idle")
        time.sleep(config["idle_seconds"])
        phase("post_idle_verification")
        if not config["no_tty"]:
            actual = call("ssmcp.ui.get", {"path": "/display/conversation"})["value"]["draft"]["text"]
            if actual != config["draft"]:
                raise RuntimeError("unsent TUI draft changed during the measured workload")
        gathered = [] if config.get("catalog_metrics", False) else None
        final_names = set()
        final_scan = scan(gathered, final_names)
        final_harnesses = call("ssmcp.harnesses.list")["harnesses"]
        initial_names = scanned_names[0]
        observed_names = set().union(*scanned_names)
        added = sorted(final_names - initial_names)
        removed = sorted(observed_names - final_names)
        initial_ready = {h["provider"] for h in harnesses if h["status"] == "ready"}
        final_ready = {h["provider"] for h in final_harnesses if h["status"] == "ready"}
        regressed = sorted(initial_ready - final_ready)
        transition = {
            "initial_tool_count": len(initial_names),
            "observed_before_idle_tool_count": len(observed_names),
            "final_tool_count": len(final_names),
            "added_name_count": len(added), "added_names": added,
            "removed_name_count": len(removed), "removed_names": removed,
            "addition_basis": "first scan", "removal_basis": "all scans before final idle",
            "initial_ready_providers": sorted(initial_ready),
            "final_ready_providers": sorted(final_ready), "ready_provider_regressions": regressed,
        }
        if config["providers"] == "installed":
            if removed or regressed:
                raise RuntimeError(f"installed catalog regressed: {len(removed)} lost tool names; providers {regressed}")
        elif final_scan["sha256"] != scans[-1]["sha256"] or final_names != scanned_names[-1]:
            raise RuntimeError("minimal/synthetic catalog changed after idle")
        save(root / "result.json", {"ok": True, "catalog_scans": scans,
             "draft_after_workload_verified": not config["no_tty"],
             "post_idle_catalog": final_scan, "catalog_transition": transition,
             "post_idle_harnesses": [{k: h.get(k) for k in ("provider", "status", "tools", "version")}
                                     for h in final_harnesses],
             "commands_verified": config["commands"], "command_output_bytes": byte_count,
             "harnesses": [{k: h.get(k) for k in ("provider", "status", "tools", "version")}
                           for h in harnesses]})
        phase("complete")
        if gathered is not None:
            save(root / "catalog-for-metrics.json", gathered)
    except Exception as error:
        save(root / "result.json", {"ok": False, "error": str(error)})
        raise


def catalog_metrics(catalog):
    """Estimate descriptor payload sizes only after the measured client exits."""
    raw_bytes = name_bytes = 0
    compressed = {1: 0, 6: 0}
    for tool in catalog:
        raw = json.dumps(tool, ensure_ascii=False, separators=(",", ":"), sort_keys=True).encode("utf-8")
        raw_bytes += len(raw)
        name_bytes += len(tool["name"].encode("utf-8"))
        for level in compressed:
            compressor = zlib.compressobj(level=level, wbits=-zlib.MAX_WBITS)
            compressed[level] += len(compressor.compress(raw)) + len(compressor.flush())
    return {
        "tools": len(catalog), "compact_json_utf8_bytes": raw_bytes,
        "routed_name_utf8_bytes": name_bytes,
        "raw_deflate_per_tool_bytes": {f"level_{level}": count for level, count in compressed.items()},
        "serialization": "UTF-8 JSON with sorted keys, compact separators, and unescaped Unicode",
        "method": "Independent zlib raw-deflate stream per tool; payload estimate, excluding allocator overhead",
        "computed_after_client_exit": True,
    }


def memory(pid):
    values = {}
    for name in ("status", "smaps_rollup"):
        for line in Path(f"/proc/{pid}/{name}").read_text().splitlines():
            key, _, value = line.partition(":")
            if key in ("VmRSS", "VmSwap", "VmHWM", "Rss", "Pss", "Swap", "SwapPss"):
                values[key] = int(value.split()[0])
    rss = values.get("Rss", values.get("VmRSS"))
    if rss is None:
        raise ProcessLookupError(pid)
    swap = values.get("Swap", values.get("VmSwap", 0))
    stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    cpu_seconds = (int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK")
    return {"cpu_seconds": cpu_seconds, "rss_kib": rss, "swap_kib": swap, "rss_swap_kib": rss + swap,
            "pss_kib": values.get("Pss"), "swap_pss_kib": values.get("SwapPss"),
            "resident_high_water_kib": values.get("VmHWM")}


def memory_details(pid):
    mappings, current = [], None
    for line in Path(f"/proc/{pid}/smaps").read_text().splitlines():
        key, _, value = line.partition(":")
        if "-" in line.split()[0]:
            fields = line.split(maxsplit=5)
            current = {"mapping": fields[5] if len(fields) == 6 else "[anonymous]"}
            mappings.append(current)
        elif current is not None and key in ("Rss", "Pss", "Swap", "Private_Dirty", "Shared_Clean"):
            current[key + "_kib"] = int(value.split()[0])
    return {"rollup": memory(pid), "top_mappings": sorted(
        mappings, key=lambda m: m.get("Rss_kib", 0) + m.get("Swap_kib", 0), reverse=True)[:12]}


def identity(pid):
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return fields[19] if fields[0] != "Z" else None
    except (FileNotFoundError, ProcessLookupError):
        return None


def descendants(pid):
    try:
        children = [int(p) for p in Path(f"/proc/{pid}/task/{pid}/children").read_text().split()]
    except (FileNotFoundError, ProcessLookupError):
        return {}
    found = {}
    for child in children:
        token = identity(child)
        if token is not None:
            found[child] = token
            found.update(descendants(child))
    return found


def cleanup_children(owned):
    remaining = {pid: token for pid, token in owned.items() if identity(pid) == token}
    for signum in (signal.SIGTERM, signal.SIGKILL):
        for pid, token in remaining.items():
            if identity(pid) == token:
                try:
                    os.kill(pid, signum)
                except ProcessLookupError:
                    pass
        deadline = time.monotonic() + 1
        while remaining and time.monotonic() < deadline:
            remaining = {pid: token for pid, token in remaining.items() if identity(pid) == token}
            if remaining:
                time.sleep(0.02)
    return sorted(remaining)


def summarize(samples):
    result = {}
    for phase in dict.fromkeys(s["phase"] for s in samples):
        rows = [s for s in samples if s["phase"] == phase]
        result[phase] = {"samples": len(rows)}
        timed = [row for row in rows if "cpu_seconds" in row]
        if len(timed) >= 2:
            duration = timed[-1]["elapsed_seconds"] - timed[0]["elapsed_seconds"]
            cpu = timed[-1]["cpu_seconds"] - timed[0]["cpu_seconds"]
            if duration > 0:
                result[phase]["cpu"] = {
                    "observed_seconds": round(duration, 6),
                    "user_system_seconds": round(cpu, 6),
                    "one_core_percent": round(cpu / duration * 100, 4),
                }
        for key in ("rss_kib", "swap_kib", "rss_swap_kib", "pss_kib", "resident_high_water_kib"):
            values = [s[key] for s in rows if s.get(key) is not None]
            if values:
                result[phase][key] = {"median": statistics.median(values), "max": max(values), "last": values[-1]}
    return result


def binary_identity(path):
    """Bind the report to stable file contents, not an installed path alone."""
    with path.open("rb") as source:
        before = os.fstat(source.fileno())
        digest = hashlib.file_digest(source, "sha256").hexdigest()
        after = os.fstat(source.fileno())
    current = path.stat()
    fields = ("st_dev", "st_ino", "st_size", "st_mtime_ns", "st_ctime_ns")
    signature = tuple(getattr(before, name) for name in fields)
    if any(tuple(getattr(item, name) for name in fields) != signature for item in (after, current)):
        raise RuntimeError("binary changed while its identity was being recorded")
    return {"sha256": digest, "bytes": before.st_size, "mtime_ns": before.st_mtime_ns,
            "ctime_ns": before.st_ctime_ns, "device": before.st_dev, "inode": before.st_ino}


def namespace_id(name):
    return os.stat(f"/proc/self/ns/{name}").st_ino


def run_isolated(args):
    """Mount source and candidate read-only; keep every client outside host state."""
    bwrap = shutil.which("bwrap")
    if not bwrap:
        raise RuntimeError("bubblewrap is required; no benchmark client was started")
    source = Path(__file__).resolve()
    with tempfile.TemporaryDirectory(prefix="ssmcp-benchmark-report-") as temporary:
        output = Path(temporary)
        command = [bwrap, "--unshare-user", "--unshare-pid", "--unshare-net", "--die-with-parent",
                   "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
                   "--tmpfs", "/var/tmp", "--tmpfs", "/tmp", "--dir", "/var/tmp/benchmark-bin",
                   "--ro-bind", str(args.binary), "/var/tmp/benchmark-bin/ssmcp",
                   "--ro-bind", str(source.parent), "/var/tmp/benchmark-src",
                   "--bind", str(output), "/var/tmp/benchmark-report"]
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "SSH_AUTH_SOCK",
                    "SSMCP_PLAYWRIGHT_COMMAND", "SSMCP_PLAYWRIGHT_ARGS", "SSMCP_CONTEXT_ROOTS"):
            command += ["--unsetenv", key]
        for key, suffix in (("XDG_CONFIG_HOME", "config"), ("XDG_STATE_HOME", "state"),
                            ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"),
                            ("XDG_RUNTIME_DIR", "runtime")):
            command += ["--setenv", key, "/tmp/ssmcp-benchmark-" + suffix]
        command += [sys.executable, "/var/tmp/benchmark-src/" + source.name, "--inside",
                    "--parent-mountns", str(namespace_id("mnt")),
                    "--parent-netns", str(namespace_id("net")),
                    "--parent-pidns", str(namespace_id("pid")),
                    "--binary", "/var/tmp/benchmark-bin/ssmcp",
                    "--report", "/var/tmp/benchmark-report/report.json"]
        for key, value in vars(args).items():
            if key in ("binary", "report", "inside", "parent_mountns", "parent_netns", "parent_pidns"):
                continue
            if value is None or value is False or value == "":
                continue
            command.append("--" + key.replace("_", "-"))
            if value is not True:
                command.append(str(value))
        completed = subprocess.run(command, capture_output=True, text=True, timeout=args.timeout + 45)
        report = output / "report.json"
        if report.exists() and args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_bytes(report.read_bytes())
        sys.stdout.write(completed.stdout)
        sys.stderr.write(completed.stderr)
        return completed.returncode


def verify_isolation(args):
    changed = {name: parent is not None and namespace_id(name) != parent
               for name, parent in (("mnt", args.parent_mountns), ("net", args.parent_netns),
                                    ("pid", args.parent_pidns))}
    mounts = Path("/proc/self/mountinfo").read_text().splitlines()
    private_state = all(any(line.split()[4] == path and " - tmpfs " in line for line in mounts)
                        for path in ("/var/tmp", "/tmp"))
    if not all(changed.values()) or not private_state:
        raise RuntimeError("benchmark needs private mount/network/PID namespaces and tmpfs state")
    if any(os.environ.get(key) for key in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "SSH_AUTH_SOCK")):
        raise RuntimeError("host desktop/socket environment reached the private fixture")
    runtime = Path("/tmp/ssmcp-benchmark-runtime")
    if os.environ.get("XDG_RUNTIME_DIR") != str(runtime):
        raise RuntimeError("private XDG runtime directory is required")
    runtime.mkdir(mode=0o700)
    return {"namespace_changed": changed, "private_state_tmpfs": private_state,
            "desktop_environment_removed": True, "xdg_runtime_mode": "0700"}


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--providers", choices=("minimal", "synthetic", "installed"), default="synthetic")
    parser.add_argument("--tools", type=int, default=1500)
    parser.add_argument("--description-bytes", type=int, default=2300)
    parser.add_argument("--scans", type=int, default=4)
    parser.add_argument("--catalog-metrics", action="store_true",
                        help="Estimate catalog payload compression after client exit (levels 1 and 6)")
    parser.add_argument("--commands", type=int, default=40)
    parser.add_argument("--forget-jobs", action="store_true",
                        help="Forget each completed job; default retains jobs like normal exec.run use")
    parser.add_argument("--command-bytes", type=int, default=512)
    parser.add_argument("--idle-seconds", type=float, default=4)
    parser.add_argument("--discovery-timeout", type=float, default=120)
    parser.add_argument("--timeout", type=float, default=240)
    parser.add_argument("--sample-ms", type=int, default=50)
    parser.add_argument("--max-footprint-mib", type=float)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--no-tty", action="store_true")
    parser.add_argument("--draft", default="",
                        help="Paste this unsent draft during the idle TUI phase")
    parser.add_argument("--completion-fixtures", type=int, default=0,
                        help="Create up to 1024 small reference-* files in the private workspace")
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--parent-mountns", type=int, help=argparse.SUPPRESS)
    parser.add_argument("--parent-netns", type=int, help=argparse.SUPPRESS)
    parser.add_argument("--parent-pidns", type=int, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if sys.platform != "linux":
        parser.error("this benchmark uses Linux /proc memory accounting")
    if not 1 <= args.tools <= 4000 or not 0 <= args.description_bytes <= 16000:
        parser.error("tools must be 1..4000; description-bytes must be 0..16000")
    if not 1 <= args.scans <= 32 or not 0 <= args.commands <= 2000:
        parser.error("scans must be 1..32; commands must be 0..2000")
    if not 0 <= args.command_bytes <= 30000 or not 10 <= args.sample_ms <= 1000:
        parser.error("command-bytes must be 0..30000; sample-ms must be 10..1000")
    if not 0.2 <= args.idle_seconds <= 60:
        parser.error("idle-seconds must be 0.2..60")
    if args.draft and args.no_tty:
        parser.error("--draft requires a TUI")
    if len(args.draft.encode("utf-8")) > 4096 or any(ord(c) < 32 and c not in "\n\t" for c in args.draft):
        parser.error("draft must be at most 4096 UTF-8 bytes and contain no terminal controls")
    if not 0 <= args.completion_fixtures <= 1024:
        parser.error("completion-fixtures must be 0..1024")
    args.binary = args.binary.resolve(strict=True)
    if not args.inside:
        return run_isolated(args)
    isolation = verify_isolation(args)
    binary = args.binary
    provenance = binary_identity(binary)
    fixture_path = Path(__file__).with_name("e2e_account_relay.py")
    source_paths = {"benchmark": Path(__file__).resolve(), "relay_fixture": fixture_path.resolve()}
    source_files = {name: {"path": str(path), **binary_identity(path)} for name, path in source_paths.items()}
    spec = importlib.util.spec_from_file_location("memory_account_relay", fixture_path)
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    settings = {k: v for k, v in vars(args).items()
                if k not in ("binary", "report", "inside", "parent_mountns", "parent_netns", "parent_pidns")}
    samples, started = [], time.monotonic()
    owned = {}
    with tempfile.TemporaryDirectory(prefix="ssmcp-memory-") as temporary:
        root = Path(temporary)
        bindir, workspace = root / "bin", root / "workspace"
        bindir.mkdir()
        workspace.mkdir()
        (root / "runtime").mkdir(mode=0o700)
        for index in range(args.completion_fixtures):
            (workspace / f"reference-{index:04d}.txt").write_text("Benchmark reference fixture.\n")
        (root / "config.toml").write_text("[ui]\ndashboard = true\n")
        save(root / "config.json", settings)
        save(root / "phase.json", "startup")
        if args.providers == "synthetic":
            path = bindir / "claude"
            path.write_text(f"#!{sys.executable}\nimport runpy\nrunpy.run_path({str(Path(__file__).resolve())!r})['native']()\n")
            path.chmod(0o700)
        env = {key: value for key, value in os.environ.items() if not key.startswith("SSMCP_")}
        env.update({"SSMCP_MEMORY_DIR": str(root), "SSMCP_AUTH_HOME": str(root / "auth"),
                    "SSMCP_SESSION_HOME": str(root / "sessions"),
                    "SSMCP_CLIENT_CONFIG": str(root / "config.toml"), "SSMCP_UI_CONFIG": str(root / "config.toml"),
                    "XDG_CONFIG_HOME": str(root / "config"), "XDG_STATE_HOME": str(root / "state"),
                    "XDG_DATA_HOME": str(root / "data"), "XDG_CACHE_HOME": str(root / "cache"),
                    "XDG_RUNTIME_DIR": str(root / "runtime"),
                    "TERM": "xterm-256color", "PATH": str(bindir) +
                    (os.pathsep + env.get("PATH", "") if args.providers == "installed" else "")})
        state = fixture.State()
        server = fixture.ThreadingHTTPServer(("127.0.0.1", 0), fixture.Handler)
        server.daemon_threads = True
        server.state = state
        state.origin = f"http://127.0.0.1:{server.server_address[1]}"
        server_thread = threading.Thread(target=server.serve_forever, daemon=True)
        server_thread.start()

        draft_injected = False
        ui_readiness = {"dashboard_active": False, "draft_verified": False}

        def prepare_draft(connection, phase):
            nonlocal draft_injected
            if args.no_tty:
                return

            def keyboard_ready():
                settings = termios.tcgetattr(master)
                return not settings[0] & (termios.ICRNL | termios.INLCR | termios.IGNCR) and not settings[3] & (termios.ICANON | termios.ECHO)

            fixture.wait_for(keyboard_ready, "dashboard raw keyboard", timeout=10)
            fields = Path(f"/proc/{process.pid}/stat").read_text().rsplit(")", 1)[1].split()
            if not (int(fields[4]) != 0 and fields[2] == fields[3] == fields[5] == str(process.pid)):
                raise RuntimeError("client does not own the controlling terminal's foreground process group")
            ui_readiness["controlling_terminal"] = {
                "pid": process.pid, "process_group": int(fields[2]), "session": int(fields[3]),
                "tty_device": int(fields[4]), "foreground_process_group": int(fields[5]),
            }
            if args.draft and phase == "idle":
                data = b"\x1b[200~" + args.draft.encode("utf-8") + b"\x1b[201~"
                if os.write(master, data) != len(data):
                    raise RuntimeError("partial draft input write")
                draft_injected = True

            def exact_draft():
                value = connection.call("ssmcp.ui.get", {"path": "/display/conversation"})["value"]
                return value if value.get("draft", {}).get("text") == args.draft else None

            actual = fixture.wait_for(exact_draft, "exact real conversation draft", timeout=10)
            ui_readiness.update(dashboard_active=True, draft_verified=True, keyboard_raw=True,
                                verified_draft=actual["draft"]["text"], verified_before_idle=True)
            ui_readiness.setdefault("phase_ready_at_monotonic", {})[phase] = time.monotonic()
            save(root / "ui-readiness.json", ui_readiness)

        def workload():
            connection = None
            try:
                def connected():
                    with state.lock:
                        return next(iter(state.sessions.values()), None)
                connection = fixture.wait_for(connected, "local benchmark connection", timeout=15)
                relay(root, connection, prepare_draft)
            except Exception as error:
                save(root / "result.json", {"ok": False, "error": str(error)})
            finally:
                if connection is not None:
                    connection.close()

        master = slave = None
        drain = None
        client_tail = bytearray()
        if not args.no_tty:
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 38, 120, 0, 0))

            def discard():
                try:
                    while chunk := os.read(master, 65536):
                        client_tail.extend(chunk)
                        del client_tail[:-16384]
                except OSError:
                    pass

            drain = threading.Thread(target=discard, daemon=True)
            drain.start()
        with (root / "client.log").open("wb") as logfile:
            command = [str(binary), "--no-watchdog", "--no-setup", "--no-browser",
                "--server", state.origin, "--allow", state.allow_principal,
                "--shell", "/bin/sh", "--ui-config", str(root / "config.toml")]
            if not args.no_tty:
                bootstrap = ("import fcntl,os,sys,termios; os.setsid(); "
                             "fcntl.ioctl(0,termios.TIOCSCTTY,0); os.tcsetpgrp(0,os.getpgrp()); "
                             "os.execv(sys.argv[1],sys.argv[1:])")
                command = [sys.executable, "-c", bootstrap, *command]
            process = subprocess.Popen(command, cwd=workspace, env=env, start_new_session=args.no_tty,
                stdin=slave if slave is not None else subprocess.DEVNULL,
                stdout=slave if slave is not None else logfile, stderr=slave if slave is not None else logfile)
            (root / "client.pid").write_text(str(process.pid))
            workload_thread = threading.Thread(target=workload, daemon=True)
            workload_thread.start()
            if slave is not None:
                os.close(slave)
            timed_out = False
            try:
                while process.poll() is None:
                    owned.update(descendants(process.pid))
                    elapsed = time.monotonic() - started
                    if elapsed > args.timeout:
                        timed_out = True
                        break
                    try:
                        phase = json.loads((root / "phase.json").read_text())
                        samples.append({"elapsed_seconds": round(elapsed, 3), "phase": phase, **memory(process.pid)})
                    except (FileNotFoundError, ProcessLookupError, PermissionError):
                        pass
                    time.sleep(args.sample_ms / 1000)
            finally:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
                if master is not None:
                    os.close(master)
                    drain.join(timeout=1)
                remaining = cleanup_children(owned)
                workload_thread.join(timeout=16)
                server.shutdown()
                server.server_close()
                server_thread.join(timeout=2)
            result = json.loads((root / "result.json").read_text()) if (root / "result.json").exists() else {
                "ok": False, "error": "client exited before the fixture finished"}
            if workload_thread.is_alive() or state.errors:
                result = {"ok": False, "error": "local relay fixture did not finish cleanly",
                          "fixture_errors": state.errors, "workload_thread_alive": workload_thread.is_alive()}
            if remaining:
                result = {"ok": False, "error": "owned child processes did not stop", "pids": remaining}
            if timed_out:
                result = {"ok": False, "error": "benchmark timed out"}
            if not result["ok"]:
                result["client_log_tail"] = ((root / "client.log").read_text(errors="replace")[-3000:]
                                             if args.no_tty else client_tail.decode("utf-8", errors="replace")[-3000:])
        metrics_file = root / "catalog-for-metrics.json"
        descriptor_metrics = catalog_metrics(json.loads(metrics_file.read_text())) if metrics_file.exists() else None
        checkpoints = root / "checkpoints.jsonl"
        if checkpoints.exists():
            for line in checkpoints.read_text().splitlines():
                checkpoint = json.loads(line)
                checkpoint["elapsed_seconds"] = round(checkpoint["elapsed_seconds"] - started, 3)
                samples.append(checkpoint)
            samples.sort(key=lambda sample: sample["elapsed_seconds"])
        active = [s for s in samples if s["phase"] not in ("startup", "discovery", "prepare_draft", "prepare_after_workload_idle")]
        active_max = max((s["rss_swap_kib"] for s in active), default=None)
        binary_unchanged = binary_identity(binary) == provenance
        sources_unchanged = all(binary_identity(path) == {key: value for key, value in source_files[name].items() if key != "path"}
                                for name, path in source_paths.items())
        phases = summarize(samples)
        durations_file = root / "phase-durations.jsonl"
        durations = {row["phase"]: row for row in (json.loads(line) for line in durations_file.read_text().splitlines())} if durations_file.exists() else {}
        timing_verified = all(durations.get(phase, {}).get("seconds", 0) >= args.idle_seconds
                              and phases.get(phase, {}).get("cpu", {}).get("observed_seconds", 0) > 0
                              for phase in ("idle", "after_workload_idle"))
        ready_at = ui_readiness.get("phase_ready_at_monotonic", {})
        prepared_before_idle = args.no_tty or all(phase in ready_at and durations.get(phase, {}).get("started_monotonic", 0) >= ready_at[phase]
                                                for phase in ("idle", "after_workload_idle"))
        ui_readiness["draft_preserved_after_workload"] = result.get("draft_after_workload_verified", False)
        ui_verified = args.no_tty or (ui_readiness["dashboard_active"] and ui_readiness["draft_verified"]
                                     and ui_readiness["draft_preserved_after_workload"] and (not args.draft or draft_injected))
        passed = binary_unchanged and sources_unchanged and timing_verified and prepared_before_idle and ui_verified and result["ok"] and active_max is not None and (args.max_footprint_mib is None or
                                                           active_max <= args.max_footprint_mib * 1024)
        report = {"schema": "ssmcp.client-memory.v1", "transport": "owned_loopback_http_websocket", "binary": str(binary), "providers": args.providers,
                  "binary_file": provenance, "binary_unchanged_during_run": binary_unchanged,
                  "source_files": source_files, "sources_unchanged_during_run": sources_unchanged,
                  "isolation": isolation,
                  "platform": {"system": platform.system(), "release": platform.release(), "machine": platform.machine()},
                  "memory_details_capture": "end_of_phase",
                  "cpu_measurement": "Client process user+system time from /proc/PID/stat; children excluded; 100% is one core",
                  "cpu_clock_ticks_per_second": os.sysconf("SC_CLK_TCK"),
                  "draft_injected": draft_injected,
                  "ui_readiness": ui_readiness,
                  "tty": not args.no_tty, "measurement": "client process RSS + swap; child runtimes excluded",
                  "units": "KiB", "sample_interval_ms": args.sample_ms, "settings": settings,
                  "elapsed_seconds": round(time.monotonic() - started, 3), "client_exit_code": process.returncode,
                  "workload": result, "catalog_payload_estimates": descriptor_metrics,
                  "phases": phases, "phase_durations": durations,
                  "timing_verified": timing_verified, "prepared_before_idle": prepared_before_idle,
                  "budget_mib": args.max_footprint_mib,
                  "memory_details": {p.stem.removeprefix("mappings-"): json.loads(p.read_text())
                                     for p in root.glob("mappings-*.json")},
                  "sampled_peak_rss_kib": max((s["rss_kib"] for s in samples), default=None),
                  "sampled_peak_rss_swap_kib": max((s["rss_swap_kib"] for s in samples), default=None),
                  "kernel_peak_rss_kib": max((s["resident_high_water_kib"] or 0 for s in samples), default=None),
                  "steady_and_workload_max_rss_swap_kib": active_max, "passed": passed}
    encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(encoded)
    print(encoded, end="")
    return 0 if passed else 1


if __name__ == "__main__":
    raise SystemExit(main())
