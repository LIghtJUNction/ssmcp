#!/usr/bin/env python3
"""Linux client RSS benchmark using a local fake SSH transport and no model calls.

Run with --binary target/release/ssmcp. --providers chooses a minimal client,
a synthetic paginated MCP catalog (default), or installed native providers.
Foreground TUI is measured at 120x38; --no-tty measures redirected output.
--max-footprint-mib gates sampled RSS + swap after discovery, through catalog
scans, verified commands, and subsequent idle. Startup peak is also reported.
Use --idle-seconds 35 to exercise the installed Codex 30-second catalog refresh;
A final catalog scan checks stable fixtures and preservation of installed tools.
Completed jobs are retained by default; --forget-jobs reproduces older benchmarks.
The paginated search workload filters pages like code_mode's server-side helper;
the helper itself and native child processes are outside the client RSS metric.
"""
from __future__ import annotations

import argparse
import base64
import fcntl
import hashlib
import json
import os
import platform
from pathlib import Path
import pty
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


def relay():
    root = directory()
    config = json.loads((root / "config.json").read_text())
    identifier = 0
    current_phase = "startup"

    def phase(name):
        nonlocal current_phase
        if current_phase not in ("startup", "discovery"):
            pid = int((root / "client.pid").read_text())
            checkpoint = {"elapsed_seconds": time.monotonic(), "phase": current_phase, **memory(pid)}
            with (root / "checkpoints.jsonl").open("a") as stream:
                stream.write(json.dumps(checkpoint) + "\n")
            if current_phase in ("idle", "after_workload_idle"):
                save(root / f"mappings-{current_phase}.json", memory_details(pid))
        save(root / "phase.json", name)
        current_phase = name

    def request(operation):
        nonlocal identifier
        identifier += 1
        send({"type": "call", "id": identifier, "operation": operation})
        while True:
            line = sys.stdin.readline()
            if not line:
                raise RuntimeError("client closed the local transport")
            response = json.loads(line)
            if response.get("type") == "steering":
                if hello.get("version", 0) < 7 or response.get("command") != {"op": "snapshot"}:
                    raise RuntimeError("unexpected steering command")
                send({"type": "steering_reply", "id": response["id"], "result": {"Ok": {
                    "schema_version": 1, "client_id": "b" * 64, "revision": 0,
                    "agents": [], "messages": []}}})
                continue
            if response.get("type") != "reply" or response.get("id") != identifier:
                raise RuntimeError("unexpected reply envelope")
            result = response["result"]
            if "Err" in result:
                raise RuntimeError(result["Err"])
            return result["Ok"]

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
        hello = json.loads(sys.stdin.readline())
        if hello.get("type") != "hello":
            raise RuntimeError("expected client hello")
        phase("discovery")
        send({"type": "ready", "url": "https://ssmcp.example/memory", "login_code": "a" * 64,
              "login_expires_in": 600, "client_id": "b" * 64})
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
        phase("after_workload_idle")
        time.sleep(config["idle_seconds"])
        phase("post_idle_verification")
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
    return {"rss_kib": rss, "swap_kib": swap, "rss_swap_kib": rss + swap,
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
    binary = args.binary.resolve(strict=True)
    provenance = binary_identity(binary)
    help_text = subprocess.run([str(binary), "--help"], capture_output=True, text=True, check=True).stdout
    ssh_flag = ["--ssh"] if "--ssh " in help_text else []
    settings = {k: v for k, v in vars(args).items() if k not in ("binary", "report")}
    samples, started = [], time.monotonic()
    owned = {}
    with tempfile.TemporaryDirectory(prefix="ssmcp-memory-") as temporary:
        root = Path(temporary)
        bindir, workspace = root / "bin", root / "workspace"
        bindir.mkdir()
        workspace.mkdir()
        (root / "config.toml").write_text("[ui]\ndashboard = true\n")
        save(root / "config.json", settings)
        save(root / "phase.json", "startup")
        for name, handler in (("ssh", "relay"), ("claude", "native")):
            if name == "claude" and args.providers != "synthetic":
                continue
            path = bindir / name
            path.write_text(f"#!{sys.executable}\nimport runpy\nrunpy.run_path({str(Path(__file__).resolve())!r})[{handler!r}]()\n")
            path.chmod(0o700)
        env = os.environ.copy()
        env.update({"SSMCP_MEMORY_DIR": str(root), "SSMCP_AUTH_HOME": str(root / "auth"),
                    "SSMCP_CLIENT_CONFIG": str(root / "config.toml"), "SSMCP_UI_CONFIG": str(root / "config.toml"),
                    "TERM": "xterm-256color", "PATH": str(bindir) +
                    (os.pathsep + env.get("PATH", "") if args.providers == "installed" else "")})
        for key in ("SSMCP_PLAYWRIGHT_COMMAND", "SSMCP_PLAYWRIGHT_ARGS", "SSMCP_CONTEXT_ROOTS"):
            env.pop(key, None)
        master = slave = None
        drain = None
        if not args.no_tty:
            master, slave = pty.openpty()
            fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 38, 120, 0, 0))

            def discard():
                try:
                    while os.read(master, 65536):
                        pass
                except OSError:
                    pass

            drain = threading.Thread(target=discard, daemon=True)
            drain.start()
        with (root / "client.log").open("wb") as logfile:
            process = subprocess.Popen([str(binary), *ssh_flag, "--no-watchdog", "--no-setup",
                "--shell", "/bin/sh", "--ui-config", str(root / "config.toml"), "memory@fixture"], cwd=workspace, env=env, start_new_session=True,
                stdin=slave if slave is not None else subprocess.DEVNULL,
                stdout=slave if slave is not None else logfile, stderr=slave if slave is not None else logfile)
            (root / "client.pid").write_text(str(process.pid))
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
            result = json.loads((root / "result.json").read_text()) if (root / "result.json").exists() else {
                "ok": False, "error": "client exited before the fixture finished"}
            if remaining:
                result = {"ok": False, "error": "owned child processes did not stop", "pids": remaining}
            if timed_out:
                result = {"ok": False, "error": "benchmark timed out"}
            if not result["ok"] and args.no_tty:
                result["client_log_tail"] = (root / "client.log").read_text(errors="replace")[-3000:]
        metrics_file = root / "catalog-for-metrics.json"
        descriptor_metrics = catalog_metrics(json.loads(metrics_file.read_text())) if metrics_file.exists() else None
        checkpoints = root / "checkpoints.jsonl"
        if checkpoints.exists():
            for line in checkpoints.read_text().splitlines():
                checkpoint = json.loads(line)
                checkpoint["elapsed_seconds"] = round(checkpoint["elapsed_seconds"] - started, 3)
                samples.append(checkpoint)
            samples.sort(key=lambda sample: sample["elapsed_seconds"])
        active = [s for s in samples if s["phase"] not in ("startup", "discovery")]
        active_max = max((s["rss_swap_kib"] for s in active), default=None)
        binary_unchanged = binary_identity(binary) == provenance
        passed = binary_unchanged and result["ok"] and active_max is not None and (args.max_footprint_mib is None or
                                                           active_max <= args.max_footprint_mib * 1024)
        report = {"schema": "ssmcp.client-memory.v1", "binary": str(binary), "providers": args.providers,
                  "binary_file": provenance, "binary_unchanged_during_run": binary_unchanged,
                  "platform": {"system": platform.system(), "release": platform.release(), "machine": platform.machine()},
                  "memory_details_capture": "end_of_phase",
                  "tty": not args.no_tty, "measurement": "client process RSS + swap; child runtimes excluded",
                  "units": "KiB", "sample_interval_ms": args.sample_ms, "settings": settings,
                  "elapsed_seconds": round(time.monotonic() - started, 3), "client_exit_code": process.returncode,
                  "workload": result, "catalog_payload_estimates": descriptor_metrics,
                  "phases": summarize(samples), "budget_mib": args.max_footprint_mib,
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
