#!/usr/bin/env python3
"""Check compatible time queries and actual dashboard clocks on private loopback.

Reuses the account WebSocket and controlling-PTY fixtures. Requires prebuilt,
frozen ssmcp/vt_snapshot binaries; no Cargo, models or desktop input are used.
The outer runner always isolates the product before it is executed.
"""
import argparse
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import termios
import time
import traceback
import uuid

import e2e_account_relay as relay
from e2e_computer_controls import Session, save, until
from e2e_dashboard import sha256

DRAFT = "TIME_FIXTURE_DRAFT_保持不变"
TIME_CAPABILITY = "time_observation_v1"
SOURCES = ("e2e_time_primitives.py", "e2e_account_relay.py", "e2e_computer_controls.py", "e2e_dashboard.py")


def fingerprints():
    return {name: sha256(Path(__file__).resolve().parent / name) for name in SOURCES}


def catalog(connection):
    names, seen, cursor = {}, set(), None
    while True:
        page = connection.request({"op": "list_tools", **({"cursor": cursor} if cursor else {})})
        for tool in page["tools"]:
            assert tool["name"] not in names, "duplicate catalog name"
            names[tool["name"]] = tool
        cursor = page.get("nextCursor")
        if not cursor:
            return names
        assert cursor not in seen, "repeated catalog cursor"
        seen.add(cursor)


def raw_input(session):
    mode = termios.tcgetattr(session.master)
    return (not mode[0] & (termios.ICRNL | termios.INLCR | termios.IGNCR)
            and not mode[3] & (termios.ICANON | termios.ECHO))


def display(connection):
    value = connection.call("ssmcp.ui.get", {"path": "/display"})["value"]
    assert value["active"] is True
    return value


def divider(session, row):
    shot = session.decoder.request(op="snapshot", cells=False)
    line = shot["rows"][row]
    assert "─" in line, (row, line)
    return line


def clock_seconds(line, label):
    found = re.search(re.escape(label) + r" (\d+(?::\d{2}){1,2})", line)
    assert found, line
    parts = [int(part) for part in found[1].split(":")]
    return parts[-1] + 60 * parts[-2] + (3600 * parts[0] if len(parts) == 3 else 0)


def until_clock(sessions, session, row, label, predicate, description, timeout):
    # Rendering has a one-second cadence independent of the operation's start.
    # Wait for the decoded numeric change, not a sleep aligned to the test.
    def progressed():
        line = divider(session, row)
        if label + " " not in line:
            return None
        return line if predicate(clock_seconds(line, label)) else None

    return until(sessions, progressed, description, timeout=timeout)


def assert_draft(connection):
    value = connection.call("ssmcp.ui.get", {"path": "/display/conversation"})["value"]
    assert value["draft"]["text"] == DRAFT, value["draft"]


def failure(connection, name, arguments, expected):
    try:
        connection.call(name, arguments)
    except AssertionError as error:
        assert expected in str(error), str(error)
        return str(error)
    raise AssertionError(f"{name} unexpectedly succeeded")


def inside(args):
    for namespace in ("mnt", "net", "pid"):
        actual = os.stat(f"/proc/self/ns/{namespace}").st_ino
        assert actual != getattr(args, "parent_" + namespace), f"private {namespace} namespace required"
    mounts = Path("/proc/self/mountinfo").read_text().splitlines()
    for target in ("/tmp", "/var/tmp"):
        assert any(line.split()[4] == target and " - tmpfs " in line for line in mounts)
    for key in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "SSH_AUTH_SOCK"):
        assert not os.environ.get(key), key
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    runtime.mkdir(mode=0o700, parents=True, exist_ok=True)
    assert runtime.stat().st_mode & 0o777 == 0o700
    Path("/var/tmp", args.probe).write_text("private fixture only\n")
    assert sha256(args.binary) == args.expected_sha256
    before_sources = fingerprints()
    report = {"ok": False, "binary_sha256": args.expected_sha256, "decoder_sha256": sha256(args.decoder),
              "source_sha256": before_sources, "private_namespaces": ["mount", "network", "pid"],
              "transport": "stdlib loopback HTTP/WebSocket", "cases": [], "handshakes": [], "captures": []}
    sessions = []

    def check(name, **details):
        report["cases"].append({"name": name, "passed": True, **details})

    def capture(session, name):
        session.pump()
        shot = session.decoder.request(op="snapshot", cells=True)
        path = args.report_dir / f"{session.label}-{name}.json"
        save(path, shot)
        report["captures"].append({"name": name, "path": path.name, "sha256": sha256(path),
                                   "source": "decoded real controlling-PTY bytes"})

    try:
        with tempfile.TemporaryDirectory(prefix="ssmcp-time-e2e-") as temporary:
            root = Path(temporary)
            for acknowledged in (False, True):
                capabilities = relay.CAPABILITIES + ((TIME_CAPABILITY,) if acknowledged else ())
                label = "time-echo" if acknowledged else "time-no-echo"
                session = Session(root, label, args.binary, args.decoder, args.report_dir, capabilities=capabilities)
                sessions.append(session)
                until(sessions, lambda: raw_input(session) and session.state.connections.get(label), "real dashboard input and relay")
                connection = session.state.connections[label][0]
                stat = Path(f"/proc/{session.process.pid}/stat").read_text().rsplit(")", 1)[1].split()
                tty = {"pid": session.process.pid, "pgrp": int(stat[2]), "session": int(stat[3]),
                       "tty_device": int(stat[4]), "foreground_pgrp": int(stat[5])}
                assert tty["tty_device"] and all(tty[key] == session.process.pid for key in ("pgrp", "session", "foreground_pgrp"))
                session.write(DRAFT.encode())
                until(sessions, lambda: connection.call("ssmcp.ui.get", {"path": "/display/conversation"})["value"]["draft"]["text"] == DRAFT, "actual unsent draft")
                view = display(connection)
                row = view["panes"]["input"]["row"] - 1
                offered = session.state.capability_offers[-1]
                echoed = connection.ready["capabilities"]
                assert TIME_CAPABILITY in offered
                assert (TIME_CAPABILITY in echoed) is acknowledged
                report["handshakes"].append({"label": label, "offered": offered, "ready_echo": echoed,
                                              "wire_version": connection.hello["version"], "tty": tty, "draft_verified": True})
                tools = catalog(connection)
                time_names = sorted(name for name in tools if name.startswith("ssmcp.time."))
                if not acknowledged:
                    assert time_names == [], time_names
                    for name, arguments in (("ssmcp.time.now", {}),
                                            ("ssmcp.time.now", {"op": "interval", "start": {"unix_ms": 0}}),
                                            ("ssmcp.time.wait", {"op": "list"})):
                        failure(connection, name, arguments, "time_observation_not_negotiated:")
                    check("no_echo_hides_catalog_and_rejects_hardcoded_time_calls", names=time_names)
                    assert_draft(connection)
                    session.close()
                    sessions.remove(session)
                    continue

                assert time_names == ["ssmcp.time.now", "ssmcp.time.wait"], time_names
                assert tools["ssmcp.time.now"]["annotations"]["readOnlyHint"] is True
                assert "interval" in tools["ssmcp.time.now"]["inputSchema"]["properties"]["op"]["enum"]
                now = connection.call("ssmcp.time.now", {})
                assert re.fullmatch(r"\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d\.\d{3}Z", now["utc_rfc3339"])
                assert now["deadline_clock"] == "monotonic" and now["monotonic_scope"].startswith("clock_")
                interval = connection.call("ssmcp.time.now", {"op": "interval",
                    "start": {"rfc3339": "2026-11-20T23:59:59.999Z"},
                    "end": {"rfc3339": "2026-11-21T08:00:00.001+08:00"}})
                assert interval["interval_ms"] == 2 and interval["absolute_ms"] == 2
                reverse = connection.call("ssmcp.time.now", {"op": "interval", "start": {"unix_ms": 1001}, "end": {"unix_ms": 0}})
                assert reverse["interval_ms"] == -1001 and reverse["duration"]["milliseconds"] == 1
                failure(connection, "ssmcp.time.now", {"op": "interval", "start": {"rfc3339": "2023-02-29T00:00:00Z"}}, "invalid Gregorian")
                check("existing_names_support_utc_offsets_signed_intervals_and_strict_dates", names=time_names, interval=interval)

                # An owned sleep is the complete workload. No model executable
                # exists on the child's private PATH, and no desktop API is used.
                job = connection.call("ssmcp.exec.start", {"command": ["/bin/sh", "-c", "exec /bin/sleep 4"], "timeout_ms": 0})
                assert job["remaining_ms"] is None and job["timeout_ms"] == 0
                until(sessions, lambda: "运行 00:" in divider(session, row), "running elapsed clock")
                elapsed_frame = until_clock(sessions, session, row, "运行", lambda seconds: seconds >= 1,
                                            "running elapsed clock reaches one second", timeout=2.5)
                state = connection.call("ssmcp.exec.status", {"job_id": job["job_id"]})
                assert state["running"] and state["duration_ms"] >= 1000 and state["remaining_ms"] is None, state
                assert clock_seconds(elapsed_frame, "运行") >= 1, elapsed_frame
                native_time = [entry for entry in display(connection)["timing"]["active"] if entry["kind"] == "task"]
                assert native_time and native_time[0]["remaining_ms"] is None
                assert abs(native_time[0]["elapsed_ms"] - state["duration_ms"]) < 1000
                capture(session, "running-elapsed")
                until(sessions, lambda: not connection.call("ssmcp.exec.status", {"job_id": job["job_id"]})["running"], "owned sleep completes")
                check("untimed_job_has_live_elapsed_clock_without_a_fake_deadline", elapsed_ms=state["duration_ms"])

                job = connection.call("ssmcp.exec.start", {"command": ["/bin/sh", "-c", "exec /bin/sleep 5"], "timeout_ms": 1800})
                assert 0 < job["remaining_ms"] <= 1800 and job["deadline_clock"] == "monotonic"
                until(sessions, lambda: "剩余 00:" in divider(session, row), "real job timeout countdown")
                capture(session, "job-countdown")
                terminal_job = until(sessions, lambda: (lambda value: value if not value["running"] else None)(connection.call("ssmcp.exec.status", {"job_id": job["job_id"]})), "actual job timeout")
                assert terminal_job["timed_out"] is True and terminal_job["state"] == "timed_out"
                check("job_deadline_drives_actual_timeout_and_ui", timeout_ms=1800, result=terminal_job["state"])

                timer = connection.call("ssmcp.time.wait", {"duration_ms": 2600, "label": "私有计时器"})
                timer_id = timer["timer_id"]
                listed = connection.call("ssmcp.time.wait", {"op": "list"})
                assert [entry["timer_id"] for entry in listed["active"]] == [timer_id]
                # The completed job can leave its last countdown frame in the
                # PTY until the next draw. Establish the timer's own first frame.
                until(sessions, lambda: "计时 · 剩余 " in session.screen
                      and "剩余 00:" in divider(session, row), "owned timer countdown frame")
                first = divider(session, row)
                capture(session, "timer-countdown")
                first_seconds = clock_seconds(first, "剩余")
                later = until_clock(sessions, session, row, "剩余", lambda seconds: seconds < first_seconds,
                                    "timer countdown strictly decreases before expiry", timeout=2.2)
                assert clock_seconds(first, "剩余") > clock_seconds(later, "剩余"), (first, later)
                ended = until(sessions, lambda: (lambda value: value if value["status"] == "elapsed" else None)(connection.call("ssmcp.time.wait", {"op": "status", "timer_id": timer_id})), "timer expires")
                assert ended["elapsed_ms"] == 2600 and ended["remaining_ms"] == 0
                until(sessions, lambda: "已到期" in divider(session, row), "visible timer expiry")
                capture(session, "timer-expired")
                check("timer_counts_down_and_retains_an_expiry_receipt", timer_id=timer_id, before=first, after=later)

                absolute = connection.call("ssmcp.time.wait", {"deadline_unix_ms": 0, "label": "past absolute instant"})
                assert absolute["status"] == "elapsed" and absolute["deadline_unix_ms"] == 0
                open_timer = connection.call("ssmcp.time.wait", {"label": "elapsed timer"})
                assert open_timer["remaining_ms"] is None
                stopped = connection.call("ssmcp.time.wait", {"op": "cancel", "timer_id": open_timer["timer_id"]})
                assert stopped["status"] == "cancelled"
                failure(connection, "ssmcp.time.wait", {"duration_ms": 1, "deadline_unix_ms": 0}, "never both")
                listed = connection.call("ssmcp.time.wait", {"op": "list"})
                assert not listed["active"] and len(listed["recent"]) == 3
                assert_draft(connection)
                check("absolute_deadline_list_cancel_and_draft_remain_consistent", recent=len(listed["recent"]), draft_verified=True)
                session.close()
                sessions.remove(session)
        assert fingerprints() == before_sources
        assert sha256(args.binary) == args.expected_sha256
        report["ok"] = True
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
        report["traceback"] = traceback.format_exc()
        raise
    finally:
        for session in sessions:
            try:
                session.close()
            except Exception as error:
                report.setdefault("cleanup_errors", []).append(str(error))
                report["ok"] = False
        save(args.report_dir / "report.json", report)
        print(json.dumps(report, ensure_ascii=False), flush=True)
    if not report["ok"]:
        raise SystemExit(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--report-dir", type=Path, required=True)
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--expected-sha256", help=argparse.SUPPRESS)
    parser.add_argument("--probe", help=argparse.SUPPRESS)
    for namespace in ("mnt", "net", "pid"):
        parser.add_argument("--parent-" + namespace, type=int, help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.inside:
        inside(args)
        return
    args.binary = args.binary.resolve(strict=True)
    args.decoder = args.decoder.resolve(strict=True)
    args.report_dir = args.report_dir.resolve()
    args.report_dir.mkdir(parents=True, exist_ok=True)
    assert not (args.report_dir / "report.json").exists(), "use a fresh report directory"
    bwrap = shutil.which("bwrap")
    assert bwrap, "bubblewrap is required before any product invocation"
    expected = sha256(args.binary)
    probe = "ssmcp-time-private-" + uuid.uuid4().hex
    command = [bwrap, "--unshare-user", "--unshare-pid", "--unshare-net", "--die-with-parent",
               "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
               "--tmpfs", "/var/tmp", "--tmpfs", "/tmp",
               "--ro-bind", str(args.binary), "/var/tmp/ssmcp-time-candidate",
               "--ro-bind", str(args.decoder), "/var/tmp/vt_snapshot",
               "--ro-bind", str(Path(__file__).resolve().parent), "/var/tmp/time-scripts",
               "--bind", str(args.report_dir), "/var/tmp/report"]
    for key in sorted(set(key for key in os.environ if key.startswith("SSMCP_")) |
                      {"DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "SSH_AUTH_SOCK"}):
        command += ["--unsetenv", key]
    for key, directory in (("XDG_STATE_HOME", "state"), ("XDG_CONFIG_HOME", "config"),
                           ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"), ("XDG_RUNTIME_DIR", "runtime")):
        command += ["--setenv", key, "/tmp/time-" + directory]
    command += [sys.executable, "/var/tmp/time-scripts/e2e_time_primitives.py", "--inside",
                "--binary", "/var/tmp/ssmcp-time-candidate", "--decoder", "/var/tmp/vt_snapshot",
                "--report-dir", "/var/tmp/report", "--expected-sha256", expected, "--probe", probe]
    for namespace in ("mnt", "net", "pid"):
        command += ["--parent-" + namespace, str(os.stat(f"/proc/self/ns/{namespace}").st_ino)]
    with (args.report_dir / "runner.stdout").open("w") as stdout, (args.report_dir / "runner.stderr").open("w") as stderr:
        result = subprocess.run(command, stdout=stdout, stderr=stderr, timeout=120)
    assert not (Path("/var/tmp") / probe).exists(), "private probe escaped its namespace"
    assert sha256(args.binary) == expected, "candidate changed during validation"
    path = args.report_dir / "report.json"
    print(path.read_text() if path.exists() else (args.report_dir / "runner.stderr").read_text())
    if result.returncode:
        raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
