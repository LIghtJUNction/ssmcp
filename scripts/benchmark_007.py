#!/usr/bin/env python3
"""Measure one Linux 007 process without changing it (Python standard library).

TCP counters include application/TLS/WebSocket bytes carried by TCP, but exclude
IP/TCP/link headers and pure acknowledgements. A disappearing socket can hide its
last bytes; the report marks such intervals as a lower bound.
"""
from __future__ import annotations

import argparse
import datetime as dt
import json
import os
from pathlib import Path
import re
import shutil
import statistics
import subprocess
import time


def read_process(pid):
    root = Path("/proc") / str(pid)
    fields = (root / "stat").read_text().rsplit(")", 1)[1].split()
    if fields[0] == "Z":
        raise RuntimeError("the requested process is a zombie")
    status = {}
    for line in (root / "status").read_text().splitlines():
        name, _, value = line.partition(":")
        if name in ("VmRSS", "VmHWM", "Threads"):
            status[name] = int(value.split()[0])
    memory = {}
    memory_error = None
    try:
        for line in (root / "smaps_rollup").read_text().splitlines():
            name, _, value = line.partition(":")
            if name in ("Rss", "Pss", "Private_Clean", "Private_Dirty", "Swap", "SwapPss"):
                memory[name.lower() + "_kib"] = int(value.split()[0])
    except (PermissionError, FileNotFoundError) as error:
        memory_error = type(error).__name__
    children = set()
    for task in (root / "task").iterdir():
        try:
            children.update(int(value) for value in (task / "children").read_text().split())
        except FileNotFoundError:
            pass
    return {
        "monotonic_seconds": time.monotonic(),
        "start_ticks": int(fields[19]),
        "cpu_ticks": int(fields[11]) + int(fields[12]),
        "rss_kib": memory.get("rss_kib", status.get("VmRSS")),
        "pss_kib": memory.get("pss_kib"),
        "peak_rss_kib": status.get("VmHWM"),
        "threads": status["Threads"],
        "file_descriptors": len(list((root / "fd").iterdir())),
        "child_pids": sorted(children),
        "memory": memory,
        "smaps_error": memory_error,
    }


def tcp_counters(pid):
    executable = shutil.which("ss")
    if executable is None:
        return {"available": False, "reason": "ss is not installed", "sockets": {}}
    result = subprocess.run(
        [executable, "-tinpeH", "state", "established"],
        capture_output=True, text=True, timeout=5, check=False,
    )
    if result.returncode:
        return {"available": False, "reason": "ss failed", "sockets": {}}
    records = []
    for line in result.stdout.splitlines():
        if line and not line[0].isspace():
            records.append(line)
        elif records:
            records[-1] += " " + line.strip()
    sockets = {}
    for record in records:
        if re.search(r"\bpid=" + str(pid) + r"\b", record) is None:
            continue
        inode = re.search(r"\bino:(\d+)\b", record)
        if inode is None:
            return {"available": False, "reason": "ss did not expose socket inode", "sockets": {}}
        values = {}
        for field in ("bytes_sent", "bytes_received", "bytes_retrans"):
            match = re.search(r"\b" + field + r":(\d+)\b", record)
            values[field] = int(match[1]) if match else 0
        sockets[inode[1]] = values
    return {"available": True, "sockets": sockets}


def summary(values):
    values = [value for value in values if value is not None]
    if not values:
        return None
    return {"min": min(values), "mean": round(statistics.mean(values), 3), "max": max(values)}


def measure(pid, seconds, interval):
    start = read_process(pid)
    initial_tcp = tcp_counters(pid)
    previous_tcp = initial_tcp
    tcp_totals = {"bytes_sent": 0, "bytes_received": 0, "bytes_retrans": 0}
    created, disappeared = set(), set()
    network_errors = []
    samples = [dict(start, elapsed_seconds=0.0)]
    deadline = start["monotonic_seconds"] + seconds
    while time.monotonic() < deadline:
        time.sleep(min(interval, max(0.0, deadline - time.monotonic())))
        current = read_process(pid)
        if current["start_ticks"] != start["start_ticks"]:
            raise RuntimeError("PID was reused; measurements would mix processes")
        current["elapsed_seconds"] = round(
            current["monotonic_seconds"] - start["monotonic_seconds"], 6
        )
        samples.append(current)
        tcp = tcp_counters(pid)
        if tcp["available"] and previous_tcp["available"]:
            old, new = previous_tcp["sockets"], tcp["sockets"]
            created.update(set(new) - set(old))
            disappeared.update(set(old) - set(new))
            for inode, counters in new.items():
                for field, value in counters.items():
                    delta = value - old.get(inode, {}).get(field, 0)
                    if delta < 0:
                        network_errors.append("a TCP counter decreased")
                    else:
                        tcp_totals[field] += delta
        elif not tcp["available"]:
            network_errors.append(tcp["reason"])
        previous_tcp = tcp
    finish = samples[-1]
    elapsed = finish["monotonic_seconds"] - start["monotonic_seconds"]
    ticks_per_second = os.sysconf("SC_CLK_TCK")
    cpu_seconds = (finish["cpu_ticks"] - start["cpu_ticks"]) / ticks_per_second
    network_available = initial_tcp["available"] and previous_tcp["available"] and not network_errors
    if not initial_tcp["available"]:
        network_errors.append(initial_tcp["reason"])
    network = {
        "available": network_available,
        "scope": "TCP payload counters, including TLS and WebSocket framing; excluding IP/TCP/link headers and pure ACKs",
        "source": "ss -tinpeH state established",
        "initial_socket_count": len(initial_tcp["sockets"]),
        "final_socket_count": len(previous_tcp["sockets"]),
        "created_socket_count": len(created),
        "disappeared_socket_count": len(disappeared),
        "closed_socket_tail_unobserved": bool(disappeared),
        "errors": sorted(set(network_errors)),
        "delta": tcp_totals if network_available else None,
        "payload_bytes_per_second": (
            round((tcp_totals["bytes_sent"] + tcp_totals["bytes_received"]) / elapsed, 4)
            if network_available else None
        ),
    }
    for sample in samples:
        sample.pop("monotonic_seconds")
        sample.pop("start_ticks")
    return {
        "schema": "ssmcp.007.benchmark.v1",
        "measured_at_utc": dt.datetime.now(dt.timezone.utc).isoformat(),
        "pid": pid,
        "process_start_ticks": start["start_ticks"],
        "scope": "requested PID only; observed child PIDs are reported separately",
        "elapsed_seconds": round(elapsed, 6),
        "sample_interval_seconds": interval,
        "sample_count": len(samples),
        "clock_ticks_per_second": ticks_per_second,
        "cpu_seconds": round(cpu_seconds, 6),
        "cpu_percent_one_core": round(cpu_seconds * 100 / elapsed, 6),
        "rss_kib": summary(sample["rss_kib"] for sample in samples),
        "pss_kib": summary(sample["pss_kib"] for sample in samples),
        "threads": summary(sample["threads"] for sample in samples),
        "file_descriptors": summary(sample["file_descriptors"] for sample in samples),
        "observed_child_pids": sorted({pid for sample in samples for pid in sample["child_pids"]}),
        "network": network,
        "samples": samples,
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pid", type=int, required=True)
    parser.add_argument("--seconds", type=float, default=300)
    parser.add_argument("--interval", type=float, default=1)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.pid < 1 or not 0 < args.seconds <= 86400 or not 0.1 <= args.interval <= args.seconds:
        parser.error("PID must be positive; use 0.1 <= interval <= seconds <= 86400")
    try:
        result = measure(args.pid, args.seconds, args.interval)
    except (OSError, RuntimeError, subprocess.SubprocessError) as error:
        parser.exit(1, f"measurement failed: {error}\n")
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(result, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({key: value for key, value in result.items() if key != "samples"}, indent=2))


if __name__ == "__main__":
    main()
