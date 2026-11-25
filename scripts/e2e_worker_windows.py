#!/usr/bin/env python3
"""Exercise worker windows with real clients, a private relay and private PTYs.

Requires an already-built ssmcp client, vt_snapshot, and library router fixture.
No Cargo invocation, public service, desktop input, installed terminal or model
is used. The terminal fixture also models a launcher that exits immediately.

Example:
  python3 scripts/e2e_worker_windows.py --bin-dir /tmp/candidate/bin \
    --fixture-binary /tmp/candidate/ssmcp-tests \
    --decoder /tmp/candidate/vt_snapshot --report /tmp/worker-windows.json
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import selectors
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


CHILD_FLAG = "--ssmcp-worker-window-child"
FIXTURE_DIR_ENV = "SSMCP_E2E_WORKER_WINDOW_DIR"
ROWS, COLS = 58, 132


def long_task_text():
    section = lambda label: "".join(
        f"{label} {index:02d} 中文正文 live task text remains readable across wrapping.\n"
        for index in range(56))
    return ("TASK_BEGIN Return WINDOW_TASK_OK from this private fixture.\n"
            + section("FIRST") + "TASK_MIDDLE 独立滚动 middle of the actual task.\n"
            + section("SECOND") + "UNBROKEN_" + "中English" * 55
            + "\nTASK_END 完整正文 reached the actual final line.\n")


def apply_worker_control(message, identity, master, slave):
    if (message.get("identity") != identity or type(message.get("request_id")) is not int
            or message["request_id"] <= 0):
        raise AssertionError("private worker control does not match its actual process")
    if message.get("kind") == "keys":
        data = bytes.fromhex(message["hex"])
        if not data or len(data) > 256:
            raise AssertionError("private key input is outside its bounded size")
        if os.write(master, data) != len(data):
            raise AssertionError("partial private key input; do not replay")
        return {"control": "keys", "hex": data.hex()}
    if message.get("kind") == "resize":
        rows, cols = message["size"]
        if not all(type(value) is int and 1 <= value <= 1000 for value in (rows, cols)):
            raise AssertionError("invalid private terminal geometry")
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        actual = struct.unpack("HHHH", fcntl.ioctl(slave, termios.TIOCGWINSZ, bytes(8)))
        if actual[:2] != (rows, cols):
            raise AssertionError("private terminal did not accept its requested geometry")
        return {"control": "resize", "size": list(actual[:2])}
    raise AssertionError("unknown private worker control")


def record(directory, kind, **fields):
    path = Path(directory) / "terminal-events.jsonl"
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(descriptor, (json.dumps({"kind": kind, **fields}, ensure_ascii=False)
                              + "\n").encode())
    finally:
        os.close(descriptor)


def process_identity(pid):
    try:
        directory = Path(f"/proc/{int(pid)}")
        fields = (directory / "stat").read_text().rsplit(")", 1)[1].split()
        if fields[0] == "Z":
            return None
        return [int(pid), int(fields[19]), directory.stat().st_uid]
    except (FileNotFoundError, ProcessLookupError):
        return None


def signal_owned(identity, number):
    if (not identity or identity[0] <= 1 or identity[0] == os.getpid()
            or identity[2] != os.getuid()):
        raise AssertionError("invalid owned process identity")
    if process_identity(identity[0]) != identity:
        return
    try:
        descriptor = os.pidfd_open(identity[0])
    except ProcessLookupError:
        return
    try:
        if process_identity(identity[0]) == identity:
            try:
                signal.pidfd_send_signal(descriptor, number)
            except ProcessLookupError:
                pass
    finally:
        os.close(descriptor)


def own_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    os.tcsetpgrp(0, os.getpgrp())


def fixture_terminal(arguments):
    """Implement xterm's argv boundary; never interpret a command string."""
    directory = Path(os.environ[FIXTURE_DIR_ENV])
    record(directory, "launcher", identity=process_identity(os.getpid()),
           argv=arguments, cwd=os.getcwd())
    if "-e" not in arguments:
        record(directory, "fixture_error", error="expected an xterm -e argv boundary")
        return 70
    command = arguments[arguments.index("-e") + 1:]
    if not command or CHILD_FLAG not in command:
        record(directory, "fixture_error", error="missing executable or child marker")
        return 71
    process = subprocess.Popen(
        [sys.executable, str(Path(__file__).resolve()), "--fixture-pty", *command],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    record(directory, "detached", identity=process_identity(process.pid))
    # Deliberately return before the child connects. Its supervisor is a separate
    # session, so tracking or killing this launcher alone cannot pass cleanup.
    return 0


def fixture_pty(command):
    directory = Path(os.environ[FIXTURE_DIR_ENV])
    master, slave = os.openpty()
    original = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    os.set_blocking(master, False)
    process = None
    stopping = False

    def stop(_number, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    transcript = (directory / f"worker-{os.getpid()}.pty").open("xb", buffering=0)
    try:
        process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave,
                                   preexec_fn=own_terminal)
        worker_identity = process_identity(process.pid)
        record(directory, "worker", identity=worker_identity,
               supervisor=process_identity(os.getpid()), argv=command, cwd=os.getcwd())
        interrupted = False
        with selectors.DefaultSelector() as selector:
            selector.register(master, selectors.EVENT_READ)
            while process.poll() is None:
                if stopping:
                    process.terminate()
                    break
                control = directory / "worker-control.json"
                if control.is_file():
                    # Keep pending output on the old side of the resize receipt.
                    while True:
                        try:
                            pending = os.read(master, 65536)
                        except BlockingIOError:
                            break
                        if not pending:
                            break
                        transcript.write(pending)
                    message = json.loads(control.read_text())
                    applied = apply_worker_control(message, worker_identity, master, slave)
                    control.unlink()
                    record(directory, "worker_control", request_id=message["request_id"],
                           identity=worker_identity, stream_offset=transcript.tell(), **applied)
                if (directory / "close-monitor").is_file() and not interrupted:
                    os.write(master, b"\x03")
                    interrupted = True
                    record(directory, "monitor_interrupt")
                reload_request = directory / "reload-monitor"
                if reload_request.is_file():
                    reload_request.unlink()
                    os.write(master, b"\x12\x12")
                    record(directory, "monitor_reload")
                if selector.select(0.025):
                    try:
                        chunk = os.read(master, 65536)
                    except OSError as error:
                        if error.errno in (errno.EIO, errno.EAGAIN):
                            continue
                        raise
                    if chunk:
                        transcript.write(chunk)
            try:
                code = process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                code = process.wait(timeout=5)
                record(directory, "fixture_cleanup", reason="supervisor termination timed out")
            record(directory, "worker_exited", identity=worker_identity, exit_code=code,
                   terminal_restored=termios.tcgetattr(slave) == original)
            return 0
    except BaseException as error:
        record(directory, "fixture_error", error=f"{type(error).__name__}: {error}")
        raise
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait(timeout=5)
            record(directory, "fixture_cleanup", reason="exception")
        transcript.close()
        os.close(master)
        os.close(slave)


if len(sys.argv) > 1 and sys.argv[1] == "--fixture-terminal":
    raise SystemExit(fixture_terminal(sys.argv[2:]))
if len(sys.argv) > 1 and sys.argv[1] == "--fixture-pty":
    raise SystemExit(fixture_pty(sys.argv[2:]))
if len(sys.argv) > 1 and sys.argv[1] == "--fixture-clipboard":
    record(Path(os.environ[FIXTURE_DIR_ENV]), "clipboard", argv=sys.argv[2:], text=sys.stdin.read())
    raise SystemExit(0)

from e2e import Harness, WORKER_TOOLS, wait_for
from e2e_client_ui import prompt_text
from e2e_composer import input_text
from e2e_dashboard import Decoder


CHAT_DISABLED = "chat_disabled: human chat is disabled for this client"
REPLY_UNSUPPORTED = "Native ssmcp.ui.reply is not advertised; send verifies the catalog and otherwise requires a client update."
SAFETY_REFUSAL = "native file mutation blocked by safety policy"


def directory_chat_enabled(client):
    """Read the authenticated process-directory contract; unknown roles never match."""
    if (client.get("chat_io_version") != 1 or client.get("kind") != "process"
            or "unavailable_reason" not in client
            or type(client.get("reply_native_supported")) is not bool):
        return None
    reason = client["unavailable_reason"]
    if reason == CHAT_DISABLED and client["reply_native_supported"] is False:
        return False
    if ((reason is None and client["reply_native_supported"] is True)
            or (reason == REPLY_UNSUPPORTED and client["reply_native_supported"] is False)):
        return True
    return None


def directory_pair(clients, workspace, excluding=()):
    current = [client for client in clients
               if client.get("cwd") == str(workspace) and client["id"] not in excluding]
    parents = [client for client in current if directory_chat_enabled(client) is True]
    children = [client for client in current if directory_chat_enabled(client) is False]
    if len(parents) == 1 and len(children) == 1 and len(current) == 2:
        return parents[0], children[0]
    return None


def digest(path):
    result = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            result.update(chunk)
    return result.hexdigest()


def events(directory):
    path = directory / "terminal-events.jsonl"
    if not path.exists():
        return []
    lines = path.read_text().splitlines(keepends=True)
    return [json.loads(line) for line in lines if line.endswith("\n")]


def empty_composer_ready(shot):
    # Do not hard-code the number of canvas columns: retired history views
    # change that count without changing whether the parent can accept input.
    return (shot["alternate_screen"] and "Message the agent" in prompt_text(shot)
            and not shot["hide_cursor"])


def region_text(frame, region):
    return "\n".join("".join(cell["text"] for cell in row[region["col"]:region["col"] + region["cols"]])
                     for row in frame["cells"][region["row"]:region["row"] + region["rows"]])


class Frontend:
    def __init__(self, harness, label, command, environment, cwd):
        self.decoder = Decoder(harness.decoder_binary)
        self.decoder.request(op="resize", rows=ROWS, cols=COLS)
        self.master, self.slave = os.openpty()
        self.original = termios.tcgetattr(self.slave)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        os.set_blocking(self.master, False)
        self.transcript = bytearray()
        self.stdout = bytearray()
        self.process = subprocess.Popen(
            command, stdin=self.slave, stdout=subprocess.PIPE, stderr=self.slave,
            cwd=cwd, env=environment, preexec_fn=own_terminal,
        )
        harness.processes.append(self.process)
        harness.frontends.append(self)
        self.label = label
        self.closed = False
        os.set_blocking(self.process.stdout.fileno(), False)

    def pump(self, timeout=0.04):
        with selectors.DefaultSelector() as selector:
            selector.register(self.master, selectors.EVENT_READ, "pty")
            selector.register(self.process.stdout, selectors.EVENT_READ, "stdout")
            for key, _ in selector.select(timeout):
                try:
                    data = os.read(key.fd, 65536)
                except OSError as error:
                    if error.errno in (errno.EIO, errno.EAGAIN):
                        continue
                    raise
                if key.data == "pty":
                    self.transcript.extend(data)
                    self.decoder.feed(data)
                else:
                    self.stdout.extend(data)

    def frame(self):
        self.pump()
        return self.decoder.request(op="snapshot", cells=True)

    def until(self, predicate, description, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            frame = self.frame()
            if predicate(frame):
                return frame
            if self.process.poll() is not None:
                raise AssertionError(f"{description}: parent exited {self.process.returncode}")
        raise AssertionError(f"{description}: no matching physical frame")

    def send(self, data):
        os.write(self.master, data)

    def request_exit(self):
        # The ordinary chat frontend requires two Ctrl-C presses on an empty
        # composer. The worker monitor has its own single-press exit path.
        self.send(b"\x01\x0b\x03\x03")

    def wait_exit(self, timeout=15):
        deadline = time.monotonic() + timeout
        while self.process.poll() is None and time.monotonic() < deadline:
            self.pump()
        if self.process.poll() is None:
            raise AssertionError("parent did not exit within the cleanup deadline")
        self.pump(0.05)
        return self.process.returncode

    def close(self):
        if self.closed:
            return
        if self.process.poll() is None:
            self.request_exit()
            try:
                self.wait_exit()
            except AssertionError:
                Harness.stop(self.process)
        self.pump(0.02)
        self.decoder.close()
        self.process.stdout.close()
        os.close(self.master)
        os.close(self.slave)
        self.closed = True


class ChildView:
    """Observe and drive only the real PTY owned by this private worker child."""

    def __init__(self, harness, directory, worker, participant):
        self.harness, self.directory = harness, directory
        self.worker, self.participant = worker, participant
        self.receipt = next(entry for entry in events(directory) if entry["kind"] == "worker")
        self.identity = self.receipt["identity"]
        self.path = directory / f"worker-{self.receipt['supervisor'][0]}.pty"
        self.decoder = Decoder(harness.decoder_binary)
        self.decoder.request(op="resize", rows=ROWS, cols=COLS)
        self.offset, self.sequence, self.resize_count = 0, 0, 0
        self.geometry = [ROWS, COLS]

    def frame(self):
        # Parent output has its own PTY. Drain it without using it as child
        # evidence, so terminal backpressure cannot stall the live IPC link.
        for frontend in self.harness.frontends:
            if not frontend.closed:
                frontend.pump(0)
        data = self.path.read_bytes()
        # Receipts are written before new-geometry bytes. Read them after the
        # byte snapshot so every resize affecting those bytes is available.
        resizes = [entry for entry in events(self.directory)
                   if entry["kind"] == "worker_control" and entry["identity"] == self.identity
                   and entry["control"] == "resize"]
        for receipt in resizes[self.resize_count:]:
            boundary = receipt["stream_offset"]
            if boundary > len(data):
                break
            if boundary < self.offset:
                raise AssertionError("late resize receipt would misdecode the child PTY")
            self.decoder.feed(data[self.offset:boundary])
            self.offset = boundary
            self.geometry = receipt["size"]
            self.decoder.request(op="resize", rows=self.geometry[0], cols=self.geometry[1])
            self.resize_count += 1
        self.decoder.feed(data[self.offset:])
        self.offset = len(data)
        return self.decoder.request(op="snapshot", cells=True)

    def until(self, predicate, description):
        def observe():
            if process_identity(self.identity[0]) != self.identity:
                raise AssertionError(f"{description}: original worker exited")
            frame = self.frame()
            return frame if predicate(frame) else None
        return wait_for(observe, timeout=15, description=description)

    def panel(self, predicate=lambda panel: True, description="worker viewport"):
        def observe():
            result = self.harness.bound(self.worker["id"], self.participant, "ssmcp.ui.get",
                                        {"path": "/display/worker_panels"})["value"]
            panel = next((value for value in result if value.get("selected")), None)
            return panel if panel is not None and predicate(panel) else None
        return wait_for(observe, timeout=15, description=description)

    def control(self, kind, **fields):
        self.frame()
        self.sequence += 1
        message = {"request_id": self.sequence, "identity": self.identity, "kind": kind, **fields}
        path = self.directory / "worker-control.json"
        if path.exists():
            raise AssertionError("previous private input is unacknowledged; do not replay")
        temporary = path.with_suffix(".tmp")
        temporary.write_text(json.dumps(message) + "\n")
        os.replace(temporary, path)
        receipt = wait_for(lambda: next((entry for entry in events(self.directory)
                           if entry["kind"] == "worker_control"
                           and entry["identity"] == self.identity
                           and entry["request_id"] == self.sequence), None),
                           timeout=10, description="private worker input acknowledgement")
        if kind == "resize":
            self.frame()
        return receipt

    def keys(self, data):
        return self.control("keys", hex=data.hex())

    def resize(self, rows, cols):
        receipt = self.control("resize", size=[rows, cols])
        wait_for(lambda: self.harness.bound(self.worker["id"], self.participant, "ssmcp.ui.get",
                 {"path": "/display/rows"})["value"] == rows,
                 timeout=15, description="worker accepts the actual PTY height")
        self.panel(lambda panel: panel["cols"] == cols and panel["rows"] <= rows,
                   "worker compositor observes the real PTY resize")
        return receipt

    def find_text(self, marker):
        for _ in range(96):
            panel = self.panel()
            frame = self.frame()
            previous = region_text(frame, panel["logs"])
            if marker in "".join(line.rstrip() for line in previous.splitlines()):
                return frame
            first = panel["logs"]["first_line"]
            self.keys(b"\x1b[6~")
            changed = self.panel(lambda value: value["logs"]["first_line"] != first
                                 or value["logs"]["following"], "next actual child page")
            if changed["logs"]["first_line"] == first:
                break
            self.until(lambda shot: region_text(shot, changed["logs"]) != previous,
                       "page movement reaches physical child cells")
        raise AssertionError(f"{marker} was not reachable in the actual child viewport")

    def save(self, label, frame=None):
        frame = frame or self.frame()
        destination = self.harness.evidence_directory
        prefix = destination / label
        transcript = destination / f"child-{self.identity[0]}.pty"
        transcript.write_bytes(self.path.read_bytes()[:self.offset])
        prefix.with_suffix(".json").write_text(json.dumps(frame, ensure_ascii=False) + "\n")
        prefix.with_suffix(".txt").write_text("\n".join(frame["rows"]) + "\n")
        controls = [entry for entry in events(self.directory)
                    if entry["kind"] == "worker_control" and entry["identity"] == self.identity]
        geometry = {"identity": self.identity, "supervisor": self.receipt["supervisor"],
                    "size": frame["size"], "transcript_offset": self.offset, "controls": controls}
        prefix.with_suffix(".geometry.json").write_text(json.dumps(geometry) + "\n")
        png = prefix.with_suffix(".png")
        render = subprocess.run(
            [sys.executable, "-B", str(Path(__file__).with_name("render_ui_snapshot.py")),
             str(prefix.with_suffix(".json")), str(png), *self.harness.font_arguments],
            capture_output=True, text=True, timeout=30)
        self.harness.check(render.returncode == 0 and png.is_file(),
                           f"{label}: PNG rendered from actual child PTY cells: {render.stderr[-300:]}")
        self.harness.child_evidence.append({
            "label": label, **geometry, "transcript": str(transcript),
            "transcript_prefix_sha256": digest(transcript),
            "cells": str(prefix.with_suffix(".json")), "screenshot": str(png),
            "screenshot_sha256": digest(png)})
        return frame

    def close(self):
        self.decoder.close()


class WorkerWindows(Harness):
    def __init__(self, directory, binary_dir, decoder, fixture_binary, evidence_directory, font_arguments):
        super().__init__(directory, binary_dir)
        self.decoder_binary = decoder
        self.fixture_binary = fixture_binary
        self.fixture_timeout_seconds = 300
        # The HTTP transport must outlive the ten-second native observation.
        self.http_timeout = 15
        self.frontends = []
        self.windows = []
        self.owned = []
        self.cases = {}
        self.cleanup_errors = []
        self.persistence_checks = []
        self.coordinators = {}
        self.worker_agents = {}
        self.clipboard_expectations = {}
        self.evidence_directory = evidence_directory
        self.font_arguments = font_arguments
        self.child_evidence = []

    def environment(self, label, directory, *, terminal=True):
        self.windows.append(directory)
        environment = self.client_environment(label)
        # The held-stdin command below uses sh. Keep it inside the fixture's
        # explicit utility directory without exposing the rest of the host PATH.
        (Path(environment["PATH"]) / "sh").symlink_to("/bin/sh")
        allowed = {"SSMCP_AUTH_HOME", "SSMCP_SESSION_HOME", "SSMCP_CLIENT_CONFIG", "SSMCP_UI_CONFIG"}
        environment = {key: value for key, value in environment.items()
                       if not key.startswith("SSMCP_") or key in allowed}
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "ZTTY_SOCKET", "TERMINAL"):
            environment.pop(key, None)
        environment.update(TERM="xterm-256color", NO_COLOR="1", COLUMNS=str(COLS), LINES=str(ROWS),
                           GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                           GIT_CONFIG_COUNT="0", PYTHONDONTWRITEBYTECODE="1")
        # All implicit user-state locations point into this fixture. In
        # particular, old conversation and context-memory implementations used
        # XDG_STATE_HOME, so a regression cannot read or alter real user data.
        for variable, folder in (("XDG_STATE_HOME", "state"), ("XDG_CONFIG_HOME", "config"),
                                 ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"),
                                 ("XDG_RUNTIME_DIR", "runtime"), ("TMPDIR", "tmp")):
            destination = directory / folder
            destination.mkdir(mode=0o700)
            environment[variable] = str(destination)
        environment[FIXTURE_DIR_ENV] = str(directory)
        clipboard = Path(environment["PATH"]) / "wl-copy"
        clipboard.write_text(
            f"#!{sys.executable}\nimport runpy, sys\n"
            f"sys.argv = [{str(Path(__file__).resolve())!r}, '--fixture-clipboard', *sys.argv[1:]]\n"
            f"runpy.run_path({str(Path(__file__).resolve())!r}, run_name='__main__')\n"
        )
        clipboard.chmod(0o700)
        # No host display socket is used. The only available clipboard program
        # is the private stdin recorder installed above.
        environment["WAYLAND_DISPLAY"] = "ssmcp-private-fixture"
        if terminal:
            folder = directory / "terminal path 'quoted' $literal"
            folder.mkdir(mode=0o700)
            launcher = folder / "xterm"
            launcher.write_text(
                f"#!{sys.executable}\nimport runpy, sys\n"
                f"sys.argv = [{str(Path(__file__).resolve())!r}, '--fixture-terminal', *sys.argv[1:]]\n"
                f"runpy.run_path({str(Path(__file__).resolve())!r}, run_name='__main__')\n"
            )
            launcher.chmod(0o700)
            environment["SSMCP_WORKER_TERMINAL"] = str(launcher)
        else:
            environment["SSMCP_WORKER_TERMINAL"] = str(directory / "absent-xterm")
        return environment

    def assert_no_persisted_conversation_state(self, stage):
        # Inspect only the test-owned tree. Terminal/service receipts and the
        # explicitly requested report are fixture artifacts, not client state.
        namespaces = {"conversations", "context-memory"}
        forbidden = sorted(str(path.relative_to(self.directory))
                           for path in self.directory.rglob("*")
                           if path.is_file()
                           and namespaces.intersection(path.relative_to(self.directory).parts))
        self.persistence_checks.append({"stage": stage, "unexpected_files": forbidden})
        self.check(not forbidden, f"{stage}: no conversation or context-memory files were created: {forbidden}")

    def assert_clipboard_counts(self):
        for directory, expected in self.clipboard_expectations.items():
            actual = len([entry for entry in events(directory) if entry["kind"] == "clipboard"])
            self.check(actual == expected,
                       "after parent exit: only the two requested non-full invitations reached the clipboard")

    def sessions(self):
        status, result, _ = self.request("GET", path="/api/client/sessions", secret=self.account_token)
        if status != 200 or not isinstance(result, dict):
            raise AssertionError("cannot read the private account's client list")
        return result["clients"]

    def select_pair(self, frontend, workspace, *, excluding=()):
        observed = []

        def connected():
            nonlocal observed
            frontend.pump()
            observed = self.sessions()
            pair = directory_pair(observed, workspace, excluding)
            if pair is not None:
                return pair
            if frontend.process.poll() is not None:
                raise AssertionError("parent exited before both client connections were ready")
            return None
        try:
            return wait_for(connected, timeout=20, description="one chat parent and one worker connection")
        except AssertionError as error:
            raise AssertionError(f"{error}; observed clients={json.dumps(observed, ensure_ascii=False)}") from error

    def join(self, client_id, name, role="main"):
        joined = self.public_call("collab", {"action": "join", "client_id": client_id,
                                           "name": name, "role": role, "capabilities": []})
        self.sensitive.append(joined["token"])
        return joined

    def bound(self, client_id, participant, name, arguments=None, *, allow_error=False,
              expected_error=None):
        if allow_error and not expected_error:
            raise AssertionError("a negative native call requires its exact expected error")
        # Native validation may reject call() before returning an MCP envelope.
        # Catch only inside the native call and validate its real message below;
        # HTTP, sandbox startup and unrelated native errors still fail this test.
        code = (
            "try { const response = await call(%s, %s, %s); return {native: response}; } "
            "catch (error) { if (!%s) throw error; "
            "return {exception: String(error.message ?? error)}; }"
        ) % (json.dumps(client_id), json.dumps(name), json.dumps(arguments or {}),
             json.dumps(allow_error))
        result = self.public_call("code_mode", {
            "agent": {"client_id": client_id, "id": participant["agent_id"], "token": participant["token"]},
            "code": code,
        })["result"]
        if "exception" in result:
            native = {"isError": True, "caught_exception": True,
                      "content": [{"type": "text", "text": result["exception"]}]}
        else:
            native = result["native"]
        if allow_error:
            if native.get("isError"):
                error = "\n".join(item.get("text", "") for item in native.get("content", []))
                if expected_error not in error:
                    raise AssertionError(f"{name} returned an unrelated error: {error}")
                self.cases.setdefault("expected_native_errors", []).append({
                    "tool": name, "error": error,
                    "caught_exception": native.get("caught_exception", False),
                })
            return native
        if native.get("isError"):
            raise AssertionError(f"{name} returned a native error: {native}")
        return self.native_value(client_id, participant, native["structuredContent"], operation=name)

    def native_value(self, client_id, participant, value, *, operation):
        if value.get("spilled") is not True:
            return value
        if operation == "ssmcp.output.read":
            raise AssertionError("native artifact reads must not recursively spill")
        meta = value.get("artifact", {})
        size = meta.get("bytes")
        evidence = {"operation": operation, "summary": value, "state": "unverified"}
        self.cases.setdefault("native_artifacts", []).append(evidence)
        if (value.get("format") != "mcp_tool_result_json" or value.get("output_error")
                or meta.get("storage") != "memory" or meta.get("path") is not None
                or meta.get("scope") != "native_tool_result_json"
                or meta.get("complete") is not True or meta.get("error")
                or not isinstance(size, int) or not 0 < size <= 1024 * 1024
                or meta.get("observed_bytes") != size
                or not re.fullmatch(r"[0-9a-f]{32}", str(meta.get("id", "")))
                or not re.fullmatch(r"[0-9a-f]{64}", str(meta.get("sha256", "")))):
            raise AssertionError("native result is not a complete bounded original memory artifact")
        payload = bytearray()
        while len(payload) < size:
            offset = len(payload)
            part = self.bound(client_id, participant, "ssmcp.output.read", {
                "id": meta["id"], "offset": offset, "max_bytes": min(16384, size - offset),
            })
            chunk = base64.b64decode(part["data_base64"], validate=True)
            end = offset + len(chunk)
            if (not chunk or end > size or part.get("id") != meta["id"]
                    or part.get("offset") != offset or part.get("next_offset") != end
                    or part.get("bytes") != len(chunk) or part.get("size") != size
                    or part.get("complete") is not True or part.get("truncated") is not False
                    or part.get("observed_bytes") != size or part.get("eof") is not (end == size)
                    or part.get("sha256_scope") != "returned_chunk"
                    or part.get("sha256") != hashlib.sha256(chunk).hexdigest()):
                raise AssertionError("native artifact returned an incomplete or mismatched chunk")
            payload.extend(chunk)
        if hashlib.sha256(payload).hexdigest() != meta["sha256"]:
            raise AssertionError("native artifact digest differs from the original retained bytes")
        native = json.loads(payload)
        if native.get("isError") or not isinstance(native.get("structuredContent"), dict):
            raise AssertionError("native artifact did not contain a successful original MCP result")
        evidence.update(state="verified", bytes=len(payload), sha256=meta["sha256"],
                        original_keys=sorted(native["structuredContent"]))
        self.check(True, f"{operation}: original spilled MCP result is read completely with exact cursors and hashes")
        return native["structuredContent"]

    def native_file_safety(self, workspace, parent, worker, manager):
        sessions = ((parent, self.coordinators[parent["id"]], "parent"),
                    (worker, manager, "worker"))
        for client, participant, label in sessions:
            def native(name, arguments, *, allow_error=False):
                return self.bound(
                    client["id"], participant, name, arguments, allow_error=allow_error,
                    expected_error=SAFETY_REFUSAL if allow_error else None,
                )

            target = workspace / f"safety-{label}.txt"
            native("ssmcp.fs.write_text", {"path": target.name, "text": label})
            self.check(target.read_text() == label, f"{label}: ordinary native write succeeds")
            for path in ("../outside-safety-fixture",):
                refusal = native("ssmcp.fs.write_text", {
                    "path": path, "text": "synthetic-private-safety-value", "create_parents": True,
                }, allow_error=True)
                self.check(refusal.get("isError") is True
                           and any(item.get("text") == SAFETY_REFUSAL
                                   for item in refusal.get("content", [])),
                           f"{label}: native safety rejects before mutation")
            policy_path = f".ssmcp/safety/attempt-{label}.toml"
            policy_target = workspace / policy_path
            native("ssmcp.fs.write_text", {
                "path": policy_path, "text": "ordinary setup", "create_parents": True,
            })
            self.check(policy_target.read_text() == "ordinary setup",
                       f"{label}: policy directory has no implicit approval barrier")
            denied_dir = workspace / f"policy-blocked-{label}"
            blocked = native("ssmcp.fs.mkdir", {"path": denied_dir.name, "parents": True},
                             allow_error=True)
            self.check(blocked.get("isError") is True
                       and any(item.get("text") == SAFETY_REFUSAL
                               for item in blocked.get("content", []))
                       and not denied_dir.exists(),
                       f"{label}: project allow cannot override user approval requirement")
            self.check(not (workspace.parent / "outside-safety-fixture").exists(),
                       f"{label}: rejected traversal has no filesystem effect")
            page = native("ssmcp.fs.events", {"stream": "safety"})
            outcomes = [(row["event"]["decision"], row["event"]["outcome"])
                        for row in page["events"]]
            self.check(outcomes == [("allow", "succeeded"), ("deny", "blocked"),
                                    ("allow", "succeeded"), ("require_approval", "blocked")]
                       and all(row["event"]["rules_state"] == "active"
                               for row in page["events"])
                       and "synthetic-private-safety-value" not in json.dumps(page),
                       f"{label}: own stream contains only its redacted decisions")

    def execute_fixture_command(self, client_id, participant):
        command = [sys.executable, "-c", "print('WINDOW_COMMAND_OK')"]
        code = "return await exec.run(%s, %s);" % (
            json.dumps(client_id), json.dumps({"command": command, "wait_ms": 5000, "max_bytes": 4096}),
        )
        response = self.public_call("code_mode", {
            "agent": {"client_id": client_id, "id": participant["agent_id"], "token": participant["token"]},
            "code": code,
        })["result"]
        self.check(response.get("exit_code") == 0 and response.get("stdout", "").strip() == "WINDOW_COMMAND_OK",
                   "the assigned external worker executes a real private command successfully")
        return response

    def pool_projection(self, frontend, parent, predicate, description):
        def projected():
            frontend.pump()
            response = self.bound(parent["id"], self.coordinators[parent["id"]],
                                  "ssmcp.ui.get", {"path": "/worker_pool"})
            if response.get("path") != "/worker_pool":
                raise AssertionError("worker-pool projection did not return its requested pointer")
            value = response.get("value")
            return value if isinstance(value, dict) and predicate(value) else None
        return wait_for(projected, timeout=20, description=description)

    def copied_invite(self, frontend, directory, command):
        before = len([entry for entry in events(directory) if entry["kind"] == "clipboard"])
        frontend.send(b"\x01\x0b" + command.encode() + b"\r")

        def copied():
            frontend.pump()
            copies = [entry for entry in events(directory) if entry["kind"] == "clipboard"]
            return copies[-1] if len(copies) > before else None

        copy = wait_for(copied, timeout=10, description=f"{command} private clipboard delivery")
        self.check(copy["argv"] == ["--type", "text/plain;charset=utf-8"],
                   f"{command}: prompt reaches the private clipboard through stdin")
        frontend.send(b"\x1b")
        frontend.until(empty_composer_ready, f"main composer after {command}")
        return copy["text"]

    def register_waiting_workers(self, frontend, parent, worker):
        executors = []
        for index in range(3):
            executor = self.join(worker["id"], f"window-executor-{index + 1}", role="worker")
            executor["wait_request_id"] = f"worker-window-ready-{index + 1}"
            waiting = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
                "request_id": executor["wait_request_id"], "wait_ms": 0,
            })
            self.check(waiting["status"] == "waiting", f"external worker {index + 1} actively waits")
            executors.append(executor)
        self.pool_projection(frontend, parent,
                             lambda pool: pool["registered_workers"] == 3 and pool["idle"] == 3
                             and pool["offline"] == 0 and pool["working"] == 0,
                             "three registered waiting workers in the parent projection")
        self.check(True, "registered workers and idle stations agree after all three workers wait")
        self.worker_agents[worker["id"]] = executors
        return executors

    def invitations_and_resize(self, frontend, directory, parent, worker, manager):
        main_prompt = self.copied_invite(frontend, directory, "/invite")
        self.check(parent["id"] in main_prompt,
                   "main invitation identifies the actual parent connection")
        worker_prompt = self.copied_invite(frontend, directory, "/invite worker")
        self.check(worker["id"] in worker_prompt and "worker.wait" in worker_prompt,
                   "worker invitation identifies the linked pool and active wait workflow")
        self.clipboard_expectations[directory] = 2
        executors = self.register_waiting_workers(frontend, parent, worker)
        # Earlier projection calls used another coordinator and may have emitted
        # its reminder. This authenticated member has a fresh delivery scope.
        coordinator = self.join(parent["id"], "standby-reminder-coordinator", role="coordinator")
        reminder_arguments = {
            "agent": {"client_id": parent["id"], "id": coordinator["agent_id"], "token": coordinator["token"]},
            "code": "const response = await call(%s, %s, %s); return response;" % (
                json.dumps(parent["id"]), json.dumps("ssmcp.ui.get"), json.dumps({"path": "/worker_pool"}),
            ),
        }
        reminder_case = {}
        self.cases["standby-reminders"] = reminder_case
        standby_pool = {"client_id": worker["id"], "total": 3, "desired_total": 3, "idle": 3,
                        "working": 0, "offline": 0, "retiring": 0, "queued": 0, "registered_workers": 3,
                        "connected": True, "stopping": False, "closed": False}
        for phase in ("first", "repeated"):
            # Keep the actual outer structuredContent, before extracting result.
            outer = self.public_call("code_mode", reminder_arguments)
            reminder_case[phase] = outer
            native = outer["result"]
            projection = native.get("structuredContent", {})
            pool = projection.get("value")
            self.check(not native.get("isError") and projection.get("path") == "/worker_pool"
                       and isinstance(pool, dict)
                       and all(pool.get(key) == value for key, value in standby_pool.items()),
                       f"{phase} coordinator call observes the same actual linked pool with three standby workers")
            reminders = outer.get("worker_reminders", [])
            if phase == "first":
                self.check(isinstance(reminders, list) and len(reminders) == 1
                           and isinstance(reminders[0], dict)
                           and reminders[0].get("schema_version") == 1
                           and reminders[0].get("client_id") == parent["id"]
                           and reminders[0].get("agent_id") == coordinator["agent_id"]
                           and reminders[0].get("worker_client_id") == worker["id"]
                           and reminders[0].get("process_id") == pool["process_id"]
                           and reminders[0].get("standby") == 3,
                           "the authenticated parent coordinator receives the actual child pool standby reminder")
            else:
                self.check(reminders == [],
                           "an immediate same-state coordinator call does not repeat the standby reminder")
        before = len([entry for entry in events(directory) if entry["kind"] == "clipboard"])
        frontend.send(b"\x01\x0b/invite worker\r")
        frontend.until(lambda shot: "Worker stations are fully staffed." in "\n".join(shot["rows"]),
                       "full worker invitation refusal")
        self.check(len([entry for entry in events(directory) if entry["kind"] == "clipboard"]) == before,
                   "a full pool explains the limit without generating another invitation")
        frontend.send(b"\x1b")
        retained = frontend.until(
            lambda shot: "/invite worker" in input_text(shot) and not shot["hide_cursor"],
            "the rejected full-pool invitation remains editable",
        )
        draft = self.bound(parent["id"], self.coordinators[parent["id"]],
                           "ssmcp.ui.get", {"path": "/display/conversation/draft"})["value"]
        self.cases["full_pool_invitation_draft"] = {
            "draft": draft, "physical_composer": input_text(retained),
        }
        self.check(draft["text"] == "/invite worker" and draft["cursor"] == len("/invite worker"),
                   "a rejected invitation preserves the exact editable command and cursor")
        frontend.send(b"\x01\x0b")
        frontend.until(empty_composer_ready, "main composer after explicitly clearing the rejected invitation")
        before_resize = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]["slots"]
        waiting_slots = {(slot["slot"], slot["epoch"]) for slot in before_resize if slot["state"] == "idle"}
        self.check(len(waiting_slots) == 3, "three exact waiting station identities precede resizing")
        previous = self.pool_projection(frontend, parent, lambda pool: pool["total"] == 3,
                                        "worker resize receipt baseline").get("last_resize")
        previous_request = previous.get("request_id") if isinstance(previous, dict) else None
        frontend.send(b"\x01\x0b/worker +1\r")
        grown = self.pool_projection(frontend, parent,
                                    lambda pool: pool["total"] == 4 and pool["offline"] == 1
                                    and isinstance(pool.get("last_resize"), dict)
                                    and pool["last_resize"].get("accepted") is True
                                    and pool["last_resize"].get("target") == 4
                                    and pool["last_resize"].get("request_id") != previous_request,
                                    "one new offline worker station")
        self.check(grown["registered_workers"] == 3 and grown["idle"] == 3,
                   "growing the pool creates capacity without inventing a connected worker")
        self.check(grown["desired_total"] == 4 and grown["last_resize"].get("error") is None,
                   "growth has an accepted receipt from the actual worker pool")
        grown_slots = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]["slots"]
        offline_slots = {(slot["slot"], slot["epoch"]) for slot in grown_slots if slot["state"] == "offline"}
        self.check(len(offline_slots) == 1 and waiting_slots.isdisjoint(offline_slots),
                   "the added offline station has a distinct identity")
        frontend.send(b"\x1b\x01\x0b/worker -1\r")
        shrunk = self.pool_projection(frontend, parent,
                                      lambda pool: pool["total"] == 3 and pool["idle"] == 3
                                      and pool["offline"] == 0
                                      and isinstance(pool.get("last_resize"), dict)
                                      and pool["last_resize"].get("accepted") is True
                                      and pool["last_resize"].get("target") == 3
                                      and pool["last_resize"].get("request_id") != grown["last_resize"]["request_id"],
                                      "offline station removed before waiting workers")
        self.check(shrunk["desired_total"] == 3 and shrunk["last_resize"].get("error") is None,
                   "shrink acknowledgement belongs to the new resize request")
        state = self.bound(worker["id"], manager, "ssmcp.worker.status")
        self.check(state["worker"]["total"] == 3 and state["worker"]["idle"] == 3,
                   "resizing through the main window updates its actual linked pool")
        remaining = {(slot["slot"], slot["epoch"]) for slot in state["worker"]["slots"]}
        self.check(remaining == waiting_slots and remaining.isdisjoint(offline_slots),
                   "shrinking preserves all original waiting station identities and removes the offline addition")
        frontend.send(b"\x1b")
        frontend.until(empty_composer_ready, "main composer after worker resizing")
        return executors[0]

    def retire_cancelled_pool(self, frontend, parent, worker, manager, executor, claim, task_id):
        previous = self.pool_projection(frontend, parent, lambda pool: pool["total"] == 3,
                                        "three occupied or waiting stations before retirement")
        previous_request = previous["last_resize"]["request_id"]
        frontend.send(b"\x01\x0b/worker -3\r")
        shrinking = self.pool_projection(
            frontend, parent,
            lambda pool: pool["total"] == 3 and pool["desired_total"] == 0
            and pool["working"] == 1 and pool["retiring"] == 2
            and isinstance(pool.get("last_resize"), dict)
            and pool["last_resize"].get("accepted") is True
            and pool["last_resize"].get("target") == 0
            and pool["last_resize"].get("request_id") != previous_request,
            "zero-capacity request waits for the active task and two idle-worker acknowledgements",
        )
        self.check(shrinking["registered_workers"] == 3 and shrinking["idle"] == 0
                   and shrinking["last_resize"].get("error") is None,
                   "an accepted shrink does not claim that registered external workers have stopped")
        for idle_worker in self.worker_agents[worker["id"]]:
            if idle_worker["agent_id"] == executor["agent_id"]:
                continue
            retirement = self.bound(worker["id"], idle_worker, "ssmcp.worker.wait", {
                "request_id": idle_worker["wait_request_id"], "wait_ms": 0,
            })
            self.check(retirement["status"] == "retiring", "an idle worker receives its actual retirement receipt")
            receipt = retirement["retirement"]
            denied = self.bound(worker["id"], manager, "ssmcp.worker.release", {
                "retirement": receipt,
            }, allow_error=True,
                expected_error="worker release does not match this exact authenticated retirement receipt")
            self.check(denied.get("isError") is True, "the manager cannot acknowledge another worker's retirement")
            foreign = self.bound(worker["id"], executor, "ssmcp.worker.release", {
                "retirement": receipt,
            }, allow_error=True,
                expected_error="worker release does not match this exact authenticated retirement receipt")
            self.check(foreign.get("isError") is True,
                       "another registered worker cannot acknowledge the idle worker's retirement")
            released = self.bound(worker["id"], idle_worker, "ssmcp.worker.release", {"retirement": receipt})
            self.check(released["status"] == "released" and released["duplicate"] is False,
                       "the original idle worker releases its exact station")
            repeated = self.bound(worker["id"], idle_worker, "ssmcp.worker.release", {"retirement": receipt})
            self.check(repeated["status"] == "released" and repeated["duplicate"] is True,
                       "retrying the same retirement acknowledgement is idempotent")

        waiting = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-cancel-claim", "wait_ms": 0,
        })
        self.check(waiting["status"] == "assigned" and waiting["cancel_requested"] is True
                   and waiting["retire_requested"] is True and waiting["claim"] == claim
                   and waiting["worker"]["working"] == 1 and waiting["worker"]["total"] == 1,
                   "a busy retiring worker retains its exact claim until it acknowledges cancellation")
        self.bound(worker["id"], executor, "ssmcp.worker.complete", {"claim": claim, "status": "cancelled"})
        result = self.bound(worker["id"], manager, "ssmcp.worker.result", {"task_id": task_id})
        self.check(result["task"]["status"] == "cancelled" and result["worker"]["working"] == 0
                   and result["worker"]["retiring"] == 1 and result["worker"]["idle"] == 0,
                   "completion preserves the result while the final station awaits its worker's release")
        finished = self.condition_wait(worker, manager, claim["generation"],
                                       [{"type": "task_finished", "task_id": task_id}])
        retiring = self.condition_wait(worker, manager, claim["generation"],
                                       [{"type": "worker_idle", "slot": claim["slot"], "epoch": claim["epoch"]}])
        self.check(finished["status"] == "satisfied" and retiring["status"] == "unavailable"
                   and retiring["conditions"][0]["state"] == "unavailable",
                   "acknowledged cancellation is finished but its retiring station is never reported idle")
        completed_retry = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-cancel-claim", "wait_ms": 0,
        })
        self.check(completed_retry["status"] == "already_completed" and completed_retry["claim"] == claim,
                   "the completed request retains its receipt during retirement")
        retirement = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-retire-after-cancel", "wait_ms": 0,
        })
        self.check(retirement["status"] == "retiring", "a new wait request obtains the final retirement receipt")
        released = self.bound(worker["id"], executor, "ssmcp.worker.release", {
            "retirement": retirement["retirement"],
        })
        self.check(released["status"] == "released" and released["worker"]["total"] == 0
                   and released["worker"]["registered_workers"] == 0,
                   "capacity reaches zero only after every original worker acknowledges retirement")
        after_release = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-released-observation", "wait_ms": 0,
        })
        self.check(after_release["status"] == "released",
                   "a released worker is instructed to stop waiting rather than registering again")
        self.pool_projection(frontend, parent,
                             lambda pool: pool["total"] == 0 and pool["desired_total"] == 0
                             and pool["retiring"] == 0 and pool["registered_workers"] == 0
                             and pool.get("connected") is True,
                             "the main window observes a connected empty worker pool")
        frontend.send(b"\x1b\x01\x0b/worker +3\r")
        regrown = self.pool_projection(
            frontend, parent,
            lambda pool: pool["total"] == 3 and pool["desired_total"] == 3
            and pool["offline"] == 3 and pool["registered_workers"] == 0
            and isinstance(pool.get("last_resize"), dict)
            and pool["last_resize"].get("accepted") is True
            and pool["last_resize"].get("target") == 3
            and pool["last_resize"].get("request_id") != shrinking["last_resize"]["request_id"],
            "growth after complete retirement creates three genuinely unstaffed stations",
        )
        self.check(regrown["idle"] == 0 and regrown["working"] == 0 and regrown["retiring"] == 0,
                   "old collaboration memberships are not counted as newly waiting workers")
        obsolete = self.bound(worker["id"], manager, "ssmcp.worker.wait_for", {
            "generation": claim["generation"], "mode": "all",
            "conditions": [{"type": "worker_idle", "slot": claim["slot"], "epoch": claim["epoch"]}],
            "wait_ms": 0,
        }, allow_error=True,
            expected_error="worker wait_for slot and epoch do not identify a current worker")
        unavailable = obsolete.get("structuredContent", {})
        obsolete_error = "\n".join(item.get("text", "") for item in obsolete.get("content", []))
        self.check((obsolete.get("isError") is True and "slot and epoch" in obsolete_error)
                   or (unavailable.get("status") == "unavailable"
                       and unavailable.get("conditions", [{}])[0].get("state") == "unavailable"),
                   "an old retired station is rejected or unavailable, never idle after unstaffed regrowth")
        retained = self.bound(worker["id"], manager, "ssmcp.worker.result", {"task_id": task_id})
        self.check(retained["task"]["status"] == "cancelled",
                   "retiring and replacing a station preserves the original task's confirmed result")
        frontend.send(b"\x1b")
        frontend.until(empty_composer_ready, "main composer after retirement and regrowth")

    def launch(self, label):
        directory = self.directory / label
        directory.mkdir(mode=0o700)
        workspace = directory / "workspace 'single' \"double\" $literal 中文"
        workspace.mkdir(mode=0o700)
        environment = self.environment(label, directory)
        home = directory / "safety-home"
        for root, name, content in (
            (home, "baseline.toml",
             'project_rules = true\n[operations]\n"ssmcp.fs.mkdir" = "require_approval"\n'),
            (workspace, "rules.toml", '[operations]\n"ssmcp.fs.mkdir" = "allow"\n'),
        ):
            root.mkdir(mode=0o700, exist_ok=True)
            config = root / ".ssmcp"
            config.mkdir(mode=0o700)
            safety = config / "safety"
            safety.mkdir(mode=0o700)
            (safety / name).write_text(content)
            (safety / name).chmod(0o600)
        environment["HOME"] = str(home)
        name = f"{label} 'single' \"double\" $literal 中文"
        command = self.client_command(name=name, extra_args=("--worker", "3", "--no-watchdog"))
        frontend = Frontend(self, label, command, environment, workspace)
        parent, worker = self.select_pair(frontend, workspace)
        if not self.secret:
            self.oauth_login({"client_id": parent["id"]})
        frontend.until(empty_composer_ready, "ordinary parent conversation and composer")
        self.check(parent["id"] != worker["id"], f"{label}: parent and worker have separate client identities")
        self.check(directory_chat_enabled(parent) is True and directory_chat_enabled(worker) is False,
                   f"{label}: only the parent exposes human chat")
        self.coordinators[parent["id"]] = self.join(parent["id"], "parent-coordinator")
        manager = self.join(worker["id"], "window-manager")
        self.native_file_safety(workspace, parent, worker, manager)
        state = self.bound(worker["id"], manager, "ssmcp.worker.status")
        self.check(state["worker"]["total"] == 3 and state["worker"]["offline"] == 3
                   and state["worker"]["idle"] == 0,
                   f"{label}: all three stations stay offline until external workers wait")
        records = wait_for(lambda: [entry for entry in events(directory) if entry["kind"] == "worker"],
                           timeout=5, description="actual worker process receipt")
        self.check(len(records) == 1, f"{label}: the child does not recursively open another terminal")
        child = records[0]
        self.owned.extend([child["identity"], child["supervisor"]])
        self.check(child["identity"] is not None and state["worker"]["process_id"] == child["identity"][0],
                   f"{label}: status identifies the actual worker process")
        projection = self.pool_projection(frontend, parent,
                                          lambda pool: pool.get("client_id") == worker["id"]
                                          and pool.get("connected") is True,
                                          "parent links the actual connected worker process")
        self.check(projection["process_id"] == child["identity"][0]
                   and projection["total"] == 3 and projection["registered_workers"] == 0
                   and projection["offline"] == 3 and projection["idle"] == 0,
                   f"{label}: /worker_pool carries the real process and unstaffed capacity")
        self.check(child["cwd"] == str(workspace) and worker["cwd"] == str(workspace),
                   f"{label}: the child preserves the working directory literally")
        self.check(child["argv"].count(CHILD_FLAG) == 1 and name in child["argv"],
                   f"{label}: quoted, spaced, dollar and Unicode arguments cross the terminal unchanged")
        launchers = [entry for entry in events(directory) if entry["kind"] == "launcher"]
        self.check(len(launchers) == 1, f"{label}: only one terminal launcher was called")
        launcher = launchers[0]["identity"]
        wait_for(lambda: process_identity(launcher[0]) != launcher, timeout=5,
                 description="detached terminal launcher exit")
        self.check(process_identity(child["identity"][0]) == child["identity"],
                   f"{label}: the actual worker remains live after its launcher exits")
        self.cases[label] = {"parent_pid": frontend.process.pid, "worker_pid": child["identity"][0],
                             "launcher_pid": launcher[0], "slots": 3,
                             "pool_generation": state["worker"]["generation"]}
        return frontend, directory, workspace, parent, worker, manager

    def assert_worker_reading(self, directory, worker, executor, claim, task_text):
        self.check(2048 < len(task_text.encode()) <= 16 * 1024,
                   "the real claimed task is mixed Unicode text beyond the old 256-byte summary")
        view = ChildView(self, directory, worker, executor)
        script = ("printf 'COMMAND_BEGIN\\n'; : '" + "retained_command_" * 22
                  + "'; read fixture_line; printf 'COMMAND_END\\n'; exit 0")
        started = self.bound(worker["id"], executor, "ssmcp.exec.start",
                             {"command": ["sh", "-c", script], "stdin": "pipe", "timeout_ms": 240000,
                              "title": "Observe a real pending private command in the worker panel"})
        job_id = started.get("job_id", started.get("id"))
        try:
            for index in range(32):
                self.bound(worker["id"], executor, "ssmcp.worker.report", {
                    "claim": claim, "request_id": f"viewport-observation-{index}",
                    "event": "progress", "progress": f"VIEW_LOG_{index:02d} "
                    + "The private command is held on its owned stdin while the viewport is checked. 中文 " * 2})
            view.resize(64, 112)
            for _ in range(128):
                selected = view.panel()["slot"]
                if selected == claim["slot"]:
                    break
                view.keys(b"\t")
                view.panel(lambda panel: panel["slot"] != selected, "Tab reaches the next actual station")
            self.check(view.panel()["slot"] == claim["slot"], "real Tab input selects the claimed station")
            view.keys(b"\x1b[H")
            view.panel(lambda panel: not panel["logs"]["following"] and panel["logs"]["first_line"] == 0)
            view.keys(b"\x1b[B")
            paused = view.panel(lambda panel: not panel["logs"]["following"]
                                and panel["logs"]["first_line"] > 0)
            anchor = (paused["logs"]["first_line"], paused["logs"]["following"])
            view.keys(b"\t")
            other = view.panel(lambda panel: panel["slot"] != claim["slot"])
            view.keys(b"\x1b[H")
            other = view.panel(lambda panel: not panel["logs"]["following"] and panel["logs"]["first_line"] == 0)
            other_anchor = (other["logs"]["first_line"], other["logs"]["following"])
            view.keys(b"\x1b[Z")
            restored = view.panel(lambda panel: panel["slot"] == claim["slot"])
            self.check((restored["logs"]["first_line"], restored["logs"]["following"]) == anchor,
                       "raw ShiftTab restores the original station's paused log position")
            view.keys(b"\t")
            restored_other = view.panel(lambda panel: panel["slot"] == other["slot"])
            self.check((restored_other["logs"]["first_line"], restored_other["logs"]["following"]) == other_anchor,
                       "raw Tab preserves the other station's independent position")
            view.keys(b"\x1b[Z")
            view.panel(lambda panel: panel["slot"] == claim["slot"])
            view.save("tall-paused-logs")

            for label, rows, cols in (("tall", 64, 112), ("narrow", 26, 60)):
                view.resize(rows, cols)
                original = view.panel()
                before_anchor = (original["logs"]["first_line"], original["logs"]["following"])
                view.keys(b"t")
                view.until(lambda frame: "Full task" in "\n".join(frame["rows"]), "T opens the actual full task")
                body = view.panel(lambda panel: panel["logs"]["task_view"])["logs"]
                view.keys(b"\x1b[H")
                first = view.until(lambda frame: "TASK_BEGIN" in region_text(frame, body),
                                   f"{label}: task beginning in physical cells")
                view.save(f"{label}-task-start", first)
                view.save(f"{label}-task-middle", view.find_text("TASK_MIDDLE"))
                view.keys(b"\x1b[F")
                end = view.until(lambda frame: "TASK_END" in region_text(frame, body),
                                 f"{label}: task final line in physical cells")
                incoming_progress = f"INCOMING_WHILE_READING_{label}: the real task viewport remains open."
                incoming = self.bound(worker["id"], executor, "ssmcp.worker.report", {
                    "claim": claim, "request_id": f"{label}-incoming-while-reading",
                    "event": "progress", "progress": incoming_progress})
                self.check(incoming["status"] == "recorded", f"{label}: incoming task progress is actually recorded")
                current = self.bound(worker["id"], executor, "ssmcp.worker.status")["worker"]
                current_slot = next(slot for slot in current["slots"] if slot["slot"] == claim["slot"])
                self.check(current_slot["progress"] == incoming_progress
                           and any(incoming_progress in entry["text"] for entry in current_slot["logs"]),
                           f"{label}: the original claimed station actually retains the incoming log")
                # Hidden log changes can legitimately produce no terminal diff.
                # Repaint through actual, reversible geometry changes instead.
                for height in (rows + 1, rows):
                    resized = view.resize(height, cols)
                    body = view.panel(lambda panel: panel["logs"]["task_view"])["logs"]
                    end = view.until(
                        lambda frame: view.offset > resized["stream_offset"]
                        and "Full task" in "\n".join(frame["rows"])
                        and "TASK_END" in region_text(frame, body),
                        f"{label}: real resize repaint preserves the full task after incoming logs")
                view.save(f"{label}-task-end-incoming", end)
                view.keys(b"t")
                view.until(lambda frame: "Full task" not in "\n".join(frame["rows"]),
                           f"{label}: T returns to the physical logs")
                restored = view.panel(lambda panel: not panel["logs"]["task_view"])
                self.check((restored["logs"]["first_line"], restored["logs"]["following"]) == before_anchor,
                           f"{label}: closing task detail restores its original paused log anchor")

            view.resize(32, 132)
            view.keys(b"\x1bOQ")
            view.panel(lambda panel: panel["slot"] == claim["slot"] and panel["view"] == "details"
                       and not panel["logs"]["task_view"], "F2 opens this station's diagnostic logs")
            view.keys(b"\x1b[H")
            view.panel(lambda panel: panel["logs"]["first_line"] == 0)
            short = view.find_text("sh -c [inline script]")
            short_text = region_text(short, view.panel()["logs"])
            started_lines = [line for line in short_text.splitlines()
                             if "RUN" in line and "sh -c [inline script]" in line]
            self.check(started_lines and all("exit 0" not in line for line in started_lines)
                       and "COMMAND_END" not in short_text and "[inline script]" in short_text,
                       "default log shows a short started-command summary without inventing its script's exit")
            self.check(self.bound(worker["id"], executor, "ssmcp.exec.status", {"job_id": job_id})["running"],
                       "the actual native command is still running while its started summary is inspected")
            view.save("wide-command-summary", short)
            view.keys(b"\r")
            view.panel(lambda panel: panel["logs"]["commands_expanded"])
            view.keys(b"\x1b[H")
            view.panel(lambda panel: panel["logs"]["first_line"] == 0)
            view.save("wide-command-expanded", view.find_text("COMMAND_END"))
            self.check(True, "real Enter expands the retained full command through its final sentinel")
            view.keys(b"\r")
            view.panel(lambda panel: not panel["logs"]["commands_expanded"])
            view.resize(ROWS, COLS)
        finally:
            view.close()
            self.bound(worker["id"], executor, "ssmcp.exec.write",
                       {"job_id": job_id, "data": "continue\n", "close_stdin": True, "wait_ms": 1000})
            finished = self.bound(worker["id"], executor, "ssmcp.exec.wait",
                                  {"job_id": job_id, "wait_ms": 5000})
            self.check(finished.get("exit_code") == 0,
                       "the private pending command really exits only after its stdin is released")
        self.cases["worker-reading"] = {"claim": claim, "task_bytes": len(task_text.encode()),
            "command_job_id": job_id, "child_identity": view.identity,
            "geometry": [[64, 112], [26, 60], [32, 132]], "screenshots": len(self.child_evidence)}

    def protocol(self, frontend, directory, parent, worker, manager):
        tools = [tool for tool in self.native_catalog(worker["id"])
                 if tool["name"].startswith("ssmcp.worker.")]
        self.cases["worker_catalog"] = {
            "expected": sorted(WORKER_TOOLS), "observed": [tool["name"] for tool in tools],
            "schemas": {tool["name"]: isinstance(tool.get("inputSchema"), dict) for tool in tools},
        }
        self.check({tool["name"] for tool in tools} == WORKER_TOOLS
                   and len(tools) == len(WORKER_TOOLS)
                   and all(isinstance(tool.get("inputSchema"), dict) for tool in tools),
                   "the configured worker pool advertises the exact worker tool catalog and schemas")
        attached = self.bound(worker["id"], manager, "ssmcp.worker.attach")
        self.check(attached["caller_is_manager"], "the main participant attaches to the discovered worker pool")
        executor = self.invitations_and_resize(frontend, directory, parent, worker, manager)
        members = self.public_call("collab", {"action": "list", "client_id": worker["id"]})
        self.check(any(member["agent_id"] == executor["agent_id"] and member["role"] == "worker"
                       for member in members["agents"]),
                   "the external executor explicitly registers as a worker before waiting")
        denied = self.bound(worker["id"], executor, "ssmcp.worker.attach", allow_error=True,
                            expected_error="worker pool belongs to a different authenticated manager")
        self.check(denied.get("isError") is True, "an executor cannot take over the pool manager")
        client_id = worker["id"]
        assignment_window = self.public_call("await", {
            "client_id": client_id,
            "agent": {"id": executor["agent_id"], "token": executor["token"]},
            "window_ms": 1000,
            "targets": [{"type": "worker_assignment", "request_id": executor["wait_request_id"]}],
        })
        self.check(assignment_window["status"] == "pending" and assignment_window["cursor"] == 0,
                   "the authenticated worker's first observation ends before submission")
        task_text = long_task_text()
        submitted = self.bound(worker["id"], manager, "ssmcp.worker.submit", {
            "request_id": "worker-windows-task-one", "text": task_text,
        })
        task = submitted.get("task", submitted)
        task_id = task["task_id"]
        self.check(task["status"] == "queued" and task["slot"] is None
                   and submitted["worker"]["queued"] == 1 and submitted["worker"]["working"] == 0,
                   "submit is a real queued task and does not pretend an executor has claimed it")
        self.assert_dispatch_display(frontend, parent, directory, worker, manager, task_id)
        def resume_assignment(after):
            return self.public_call("await", {
                "client_id": client_id,
                "agent": {"id": executor["agent_id"], "token": executor["token"]},
                "wait_id": assignment_window["wait_id"], "after": after,
            })

        observed = resume_assignment(0)
        self.cases["assignment-observation"] = {"first_window": assignment_window,
                                                "original_ready_response": observed}
        assignment = self.native_value(worker["id"], executor, observed["results"][0]["data"],
                                       operation="await:worker_assignment")
        self.check(isinstance(assignment.get("claim"), dict),
                   "the original assignment response contains its actionable native claim")
        self.check(observed["status"] == "ready" and observed["cursor"] == 1
                   and assignment["status"] == "assigned" and assignment["claim"]["task_id"] == task_id,
                   "one authenticated executor receives the task submitted between observation windows")
        self.check(resume_assignment(0)["results"] == observed["results"],
                   "retrying the unconfirmed observation returns exactly the same assignment")
        self.check(resume_assignment(1)["acknowledged"] is True,
                   "confirming the assignment clears its replay receipt without dispatching again")
        duplicate = self.bound(worker["id"], executor, "ssmcp.worker.claim", {
            "request_id": executor["wait_request_id"],
        })
        self.check(duplicate["claim"] == assignment["claim"], "claim retry retains the exact assigned lease")
        self.assert_dispatch_display(frontend, parent, directory, worker, manager, task_id, claim=assignment["claim"])
        self.execute_fixture_command(worker["id"], executor)
        command_state = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
        command_slot = next(slot for slot in command_state["slots"]
                            if slot["slot"] == assignment["claim"]["slot"])
        command_logs = [entry for entry in command_slot["logs"] if "python" in entry["text"].lower()]
        self.check("python" in (command_slot.get("last_command") or "").lower()
                   and bool(command_logs) and all(entry.get("task_id") == task_id for entry in command_logs),
                   "before any progress report, the real command already has a correctly attributed log")
        progress = "The private Python command returned WINDOW_COMMAND_OK with exit code 0."
        report_arguments = {"claim": assignment["claim"], "request_id": "worker-window-progress-one",
                            "event": "progress", "progress": progress}
        reported = self.bound(worker["id"], executor, "ssmcp.worker.report", report_arguments)
        self.check(reported["status"] == "recorded" and reported["duplicate"] is False,
                   "the claimant reports an observation after the command really completed")
        repeated = self.bound(worker["id"], executor, "ssmcp.worker.report", report_arguments)
        self.check(repeated["duplicate"] is True, "an identical progress retry does not create a second event")
        rejected = self.bound(worker["id"], manager, "ssmcp.worker.report", {
            **report_arguments, "request_id": "worker-window-foreign-progress", "progress": "UNAUTHORIZED_PROGRESS",
        }, allow_error=True,
            expected_error="worker activity requires this exact authenticated current claim")
        self.check(rejected.get("isError") is True, "the manager cannot report activity as another claim's executor")
        active = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
        active_slot = next(slot for slot in active["slots"] if slot["slot"] == assignment["claim"]["slot"])
        self.check("WINDOW_TASK_OK" in (active_slot.get("task_summary") or "")
                   and "python" in (active_slot.get("last_command") or "").lower()
                   and all(entry in active_slot["logs"] for entry in command_logs),
                   "monitor summary and command log are attributed to the actual claimed task")
        self.check(active_slot.get("progress") == progress
                   and sum(progress in entry["text"] for entry in active_slot["logs"]) == 1,
                   "the monitor retains one truthful, authenticated progress observation")
        self.check(all(not slot.get("last_command") for slot in active["slots"]
                       if slot["slot"] != active_slot["slot"]),
                   "other waiting workers do not inherit the active worker's command")
        elapsed = active_slot.get("current_task_elapsed_ms")
        self.check(isinstance(elapsed, int) and elapsed >= 0 and active_slot["total_work_ms"] >= elapsed,
                   "current work time is present while the real task is active")
        self.assert_worker_reading(directory, worker, executor, assignment["claim"], task_text)
        self.assert_busy_reload(directory, worker, manager, task_id)
        self.paired_condition_waits(frontend, directory, worker, manager, executor, assignment["claim"])
        result = self.bound(worker["id"], manager, "ssmcp.worker.result", {"task_id": task_id})
        task = result.get("task", result)
        self.check(task["status"] == "completed" and task["output"] == "WINDOW_TASK_OK",
                   "the main participant retrieves the exact worker result through the real relay")
        code = "return await waitFor(%s, %s);" % (
            json.dumps(client_id),
            json.dumps({"window_ms": 1000, "targets": [{"type": "worker_result", "task_id": task_id}]}),
        )
        via_code = self.public_call("code_mode", {
            "agent": {"client_id": client_id, "id": manager["agent_id"], "token": manager["token"]},
            "code": code,
        })["result"]
        self.check(via_code["status"] == "ready" and via_code["cursor"] == 1
                   and via_code["results"][0]["data"]["task"]["task_id"] == task_id
                   and via_code["results"][0]["data"]["task"]["output"] == "WINDOW_TASK_OK",
                   "real Code Mode observes the exact completed worker task through the relay")
        ack = self.public_call("await", {
            "client_id": client_id,
            "agent": {"id": manager["agent_id"], "token": manager["token"]},
            "wait_id": via_code["wait_id"], "after": 1,
        })
        self.check(ack["acknowledged"] is True,
                   "the same manager can confirm the Code Mode observation through public await")
        status = self.bound(worker["id"], manager, "ssmcp.worker.status")
        self.check(status["worker"]["idle"] == 3 and status["worker"]["working"] == 0,
                   "completion returns the child pool to three idle slots")
        completed_slot = next(slot for slot in status["worker"]["slots"]
                              if slot["slot"] == assignment["claim"]["slot"])
        durations = completed_slot["recent_task_durations_ms"]
        self.check(completed_slot["current_task_elapsed_ms"] is None
                   and len(durations) == 1 and durations[0] >= elapsed
                   and completed_slot["total_work_ms"] == durations[0],
                   "completion freezes one real task duration without a fabricated active timer")
        cancelled = self.bound(worker["id"], manager, "ssmcp.worker.submit", {
            "request_id": "worker-windows-cancel-task", "text": "Wait for cancellation in this private fixture.",
        })["task"]["task_id"]
        pending = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-cancel-claim", "wait_ms": 1000,
        })
        next_task = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
        next_slot = next(slot for slot in next_task["slots"] if slot["slot"] == pending["claim"]["slot"])
        self.check(next_slot.get("last_command") is None and next_slot.get("progress") is None,
                   "a new task clears the previous task's command and progress fields")
        self.timed_condition_waits(frontend, parent, directory, worker, manager, pending["claim"])
        cancellation = self.bound(worker["id"], manager, "ssmcp.worker.cancel", {"task_id": cancelled})
        self.check(cancellation["task"]["status"] == "cancelling"
                   and cancellation["worker"]["working"] == 1,
                   "cancellation keeps a remote task occupied until its executor acknowledges")
        cancelling = self.condition_wait(worker, manager, pending["claim"]["generation"],
                                         [{"type": "task_finished", "task_id": cancelled}])
        self.check(cancelling["status"] == "timed_out" and cancelling["conditions"][0]["state"] == "pending",
                   "requesting cancellation does not falsely satisfy task_finished before the claimant acknowledges")
        waiting = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-cancel-claim", "wait_ms": 0,
        })
        self.check(waiting["cancel_requested"] is True and waiting["claim"] == pending["claim"],
                   "the original claimant observes cancellation through the same claim")
        self.retire_cancelled_pool(frontend, parent, worker, manager, executor, pending["claim"], cancelled)
        parent_agent = self.coordinators[parent["id"]]
        receipt = self.bound(parent["id"], parent_agent, "ssmcp.ui.message", {
            "text": "WINDOW_TASK_OK", "kind": "progress",
        })
        self.check(receipt["accepted"] and receipt["recorded"], "the parent records the real worker result")
        frontend.send(b"\x1b[1;5F")
        frontend.until(lambda shot: "WINDOW_TASK_OK" in "\n".join(shot["rows"]),
                       "worker result displayed in the parent conversation")
        self.check(True, "the parent conversation receives the independently completed worker result")
        self.check_composer(frontend, "parent remains editable during coordination")

    @staticmethod
    def visible_monitor_frame(frame, labels):
        shot = frame()
        text = "\n".join(shot["rows"])
        return shot if all(label in text for label in labels) else None

    def condition_wait(self, worker, manager, generation, conditions, *, mode="all", wait_ms=0):
        return self.bound(worker["id"], manager, "ssmcp.worker.wait_for", {
            "generation": generation, "mode": mode, "conditions": conditions, "wait_ms": wait_ms,
        })

    @staticmethod
    def semantic_targets(targets):
        # Only the documented display summary is non-semantic. Preserve order,
        # cardinality and every other key, including any unexpected identity.
        return [{key: value for key, value in target.items() if key != "summary"}
                for target in targets]

    def parent_display(self, parent, path, **page):
        return self.bound(parent["id"], self.coordinators[parent["id"]], "ssmcp.ui.get",
                          {"path": f"/display/{path}", **page})

    def parent_events(self, parent):
        offset = 0
        while offset is not None:
            page = self.parent_display(parent, "events/events", offset=offset, limit=12)
            yield from page["value"]
            offset = page.get("next_offset")

    def conversation_operation(self, frontend, parent, heading, labels, *, deadline=None):
        remaining = lambda: 5 if deadline is None else max(0, deadline - time.monotonic())

        def display(path):
            return self.parent_display(parent, path)["value"]

        context_keys = ("draft", "target", "native_recipient", "editing")
        state = display("conversation")
        before = {key: state[key] for key in context_keys}
        self.check(state["view"] == "activity" and state["focus"] == "input",
                   "operation inspection starts in the actual editable parent conversation")
        # Ctrl-End follows the existing conversation without editing its draft.
        frontend.send(b"\x1b[1;5F")
        frontend.until(lambda shot: heading in "\n".join(shot["rows"]),
                       f"the actual parent conversation contains {heading}", timeout=remaining())
        events = list(self.parent_events(parent))
        candidates = [event for event in events if heading in event["summary"]]
        self.check(len(candidates) == 1, "the operation has one retained parent conversation card")
        event_id = candidates[0]["id"]
        self.check(not candidates[0]["expanded"], "the newly observed operation card starts collapsed")
        frontend.send(b"\t")
        wait_for(lambda: display("conversation")["focus"] == "content",
                 timeout=remaining(), description="Tab focuses the actual parent conversation")
        ids = [event["id"] for event in events]
        selected = display("events/selected")
        for _ in ids:
            if selected == event_id:
                break
            if selected not in ids:
                raise AssertionError("the parent operation card is outside the retained selection")
            frontend.send(b"\x1b[A" if ids.index(event_id) < ids.index(selected) else b"\x1b[B")

            def moved():
                current = display("events/selected")
                return current if current != selected else None

            selected = wait_for(moved, timeout=remaining(), description="move the parent card selection")
        self.check(selected == event_id, "keyboard navigation selects the exact operation card")
        frontend.send(b"\r")
        expanded = frontend.until(
            lambda frame: any(f"▾ {heading}" in row for row in frame["rows"]),
            "the original operation card opens in the physical terminal", timeout=remaining())
        if not all(label in "\n".join(expanded["rows"]) for label in labels):
            # An expanded card near the bottom can open below the viewport.
            frontend.send(b"\x1b[6~")
            expanded = frontend.until(
                lambda frame: all(label in "\n".join(frame["rows"]) for label in labels),
                "expanded physical operation details retain every exact reference", timeout=remaining())
        retained = self.parent_display(parent, "events/events", offset=ids.index(event_id), limit=1)["value"]
        self.check(len(retained) == 1 and retained[0]["selected"] and retained[0]["expanded"],
                   "the original operation card is selected and expanded")
        self.check(retained[0]["id"] == event_id, "the expanded card retains its original event ID")
        frontend.send(b"\x1b")
        restored = frontend.until(lambda frame: not frame["hide_cursor"],
                                  "Esc returns focus to the original editable draft", timeout=remaining())
        state = display("conversation")
        after = {key: state[key] for key in context_keys}
        self.check(after == before and state["focus"] == "input",
                   "reading operation details preserves the exact draft, cursor and recipient")
        return {"frame": expanded, "editable_frame": restored, "event": retained[0],
                "context_before": before, "context_after": after}

    def assert_dispatch_display(self, frontend, parent, directory, worker, manager, task_id, *, claim=None):
        status = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
        kind, state = ("claim", "assigned") if claim else ("submit", "submitted")
        matching = [operation for operation in status["manager_operations"]
                    if operation["kind"] == kind and operation["task_id"] == task_id]
        self.check(len(matching) == 1 and matching[0]["state"] == state,
                   f"one real {kind} operation identifies the exact task, including after retries")
        operation = matching[0]
        evidence = {"operation": operation}
        self.cases.setdefault("manager-dispatch", {})[kind] = evidence
        labels = [f"Task ID {task_id}",
                  f"Pool generation {status['generation']} · operation {operation['operation_id']}"]
        if claim:
            self.check(operation["slot"] == claim["slot"] and operation["epoch"] == claim["epoch"],
                       "assignment display identifies the actual zero-based slot and epoch")
            heading = f"Task assigned · Worker #{claim['slot'] + 1:02}"
            labels.append(f"Worker #{claim['slot'] + 1:02} · slot {claim['slot']} · epoch {claim['epoch']}")
        else:
            self.check(operation["slot"] is None and operation["epoch"] is None
                       and not any(item["kind"] == "claim" and item["task_id"] == task_id
                                   for item in status["manager_operations"]),
                       "submission does not fabricate an assigned worker or claim")
            heading = "Task submitted · Queued"
        evidence["conversation"] = self.conversation_operation(
            frontend, parent, heading, [heading, *labels])
        view = ChildView(self, directory, worker, manager)
        try:
            labels = [f"Queued {status['queued']}", f"Working {status['working']}",
                      f"Idle {status['idle']}"]
            shot = wait_for(lambda: self.visible_monitor_frame(view.frame, labels),
                            timeout=5, description=f"actual child capacity after {kind}")
        finally:
            view.close()
        evidence["frame"] = shot

    def paired_condition_waits(self, frontend, directory, worker, manager, executor, claim):
        generation, task_id = claim["generation"], claim["task_id"]
        idle_condition = {"type": "worker_idle", "slot": claim["slot"], "epoch": claim["epoch"]}
        busy = self.condition_wait(worker, manager, generation, [idle_condition])
        self.check(busy["status"] == "timed_out" and busy["conditions"][0]["state"] == "pending"
                   and busy["worker"]["idle"] == 2,
                   "another idle worker cannot satisfy the exact busy slot and epoch")
        denied = self.bound(worker["id"], executor, "ssmcp.worker.wait_for", {
            "generation": generation, "mode": "all", "conditions": [idle_condition], "wait_ms": 0,
        }, allow_error=True,
            expected_error="this worker management action requires the pool's authenticated manager")
        self.check(denied.get("isError") is True, "only the authenticated pool manager can wait for conditions")

        paired = self.bound(worker["id"], manager, "ssmcp.worker.submit", {
            "request_id": "worker-windows-task-pair", "text": "Return WINDOW_PAIR_OK in this private fixture.",
        })["task"]
        second = self.worker_agents[worker["id"]][1]
        assigned = self.bound(worker["id"], second, "ssmcp.worker.wait", {
            "request_id": second["wait_request_id"], "wait_ms": 1000,
        })
        second_claim = assigned["claim"]
        self.check(assigned["status"] == "assigned" and second_claim["task_id"] == paired["task_id"]
                   and second_claim["slot"] != claim["slot"] and second_claim["generation"] == generation,
                   "the second existing executor holds a distinct real station for the paired task")
        conditions = [{"type": "task_finished", "task_id": value}
                      for value in (task_id, paired["task_id"])]
        targets = [{"kind": "task_finished", "task_id": value}
                   for value in (task_id, paired["task_id"])]
        case = self.cases["manager-any-all"] = {"targets": targets, "busy_slot": busy}
        draft = "COORDINATION_DRAFT_STAYS_LOCAL"
        frontend.send(b"\x1b[200~" + draft.encode() + b"\x1b[201~")
        frontend.until(lambda shot: draft in input_text(shot), "draft before coordinator waits")
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as pool:
            waits = {mode: pool.submit(self.condition_wait, worker, manager, generation, conditions,
                                       mode=mode, wait_ms=5000) for mode in ("any", "all")}

            def both_waiting():
                frontend.pump()
                current = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
                pending = {item["mode"]: item for item in current["manager_operations"]
                           if item["kind"] == "wait_for" and item["state"] == "waiting"
                           and self.semantic_targets(item["targets"]) == targets}
                return pending if set(pending) == {"any", "all"} else None

            operations = wait_for(both_waiting, timeout=2, description="both actual Any and All waits")
            case["waiting"] = operations
            self.bound(worker["id"], executor, "ssmcp.worker.complete", {
                "claim": claim, "status": "completed", "output": "WINDOW_TASK_OK",
            })
            any_result = waits["any"].result(timeout=7)
            case["any"] = any_result
            self.check(any_result["status"] == "satisfied"
                       and any_result["generation"] == generation and any_result["mode"] == "any"
                       and any_result["operation_id"] == operations["any"]["operation_id"]
                       and [(item["type"], item["task_id"], item["state"])
                            for item in any_result["conditions"]]
                       == [("task_finished", task_id, "satisfied"),
                           ("task_finished", paired["task_id"], "pending")],
                       "Any returns after exactly the first task finishes")
            active = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
            remaining = next(item for item in active["manager_operations"]
                             if item["operation_id"] == operations["all"]["operation_id"])
            self.check(remaining["state"] == "waiting" and not waits["all"].done(),
                       "All remains genuinely pending while the second task is still claimed")
            self.bound(worker["id"], second, "ssmcp.worker.complete", {
                "claim": second_claim, "status": "completed", "output": "WINDOW_PAIR_OK",
            })
            all_result = waits["all"].result(timeout=7)
            case["all"] = all_result
            self.check(all_result["status"] == "satisfied"
                       and all_result["generation"] == generation and all_result["mode"] == "all"
                       and all_result["operation_id"] == operations["all"]["operation_id"]
                       and [(item["type"], item["task_id"], item["state"])
                            for item in all_result["conditions"]]
                       == [("task_finished", task_id, "satisfied"),
                           ("task_finished", paired["task_id"], "satisfied")],
                       "All returns only when both exact tasks are finished")
        parent_frame = frontend.until(
            lambda shot: draft in input_text(shot) and not shot["hide_cursor"],
            "the parent's editable draft after both coordinator waits finish", timeout=5)
        self.check(True, "coordinator waits preserve the parent's editable draft through completion")
        case["parent_frame_after_completion"] = parent_frame
        # The original retirement scenario must use a new wait identity;
        # replaying the paired assignment would correctly return its receipt.
        second["wait_request_id"] = "worker-windows-pair-ready-again"
        ready = self.bound(worker["id"], second, "ssmcp.worker.wait", {
            "request_id": second["wait_request_id"], "wait_ms": 0,
        })
        self.check(ready["status"] == "waiting", "the paired executor returns to its real idle wait")
        idle = self.condition_wait(worker, manager, generation, [idle_condition])
        self.check(idle["status"] == "satisfied" and idle["conditions"][0]["state"] == "satisfied",
                   "completion makes the original exact registered slot and epoch idle")
        current = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
        ids = {item["operation_id"] for item in operations.values()}
        ended = [item for item in current["manager_operations"] if item["operation_id"] in ids]
        self.check(len(ended) == 2 and all(item["state"] == "satisfied" for item in ended)
                   and not any(item["state"] == "waiting" for item in current["manager_operations"]),
                   "satisfied Any and All waits leave no active coordinator operation")
        case["idle_slot"] = idle
        frontend.send(b"\x01\x0b")
        frontend.until(empty_composer_ready, "parent composer after the paired condition waits")

    def timed_condition_waits(self, frontend, parent, directory, worker, manager, claim):
        generation, task_id = claim["generation"], claim["task_id"]
        wait_ms = 10_000
        conditions = [{"type": "task_finished", "task_id": task_id},
                      {"type": "worker_idle", "slot": claim["slot"], "epoch": claim["epoch"]}]
        targets = [{"kind": "task_finished", "task_id": task_id},
                   {"kind": "worker_idle", "slot": claim["slot"], "epoch": claim["epoch"]}]
        draft = "COORDINATION_WAIT_DRAFT_STAYS_LOCAL"
        frontend.send(b"\x1b[200~" + draft.encode() + b"\x1b[201~")
        frontend.until(lambda shot: draft in input_text(shot), "draft before the visible bounded wait")
        view = ChildView(self, directory, worker, manager)
        case = self.cases["manager-wait-deadlines"] = {}
        try:
            view.frame()
            with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
                for outer_timeout in (False, True):
                    previous = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
                    known = {item["operation_id"] for item in previous["manager_operations"]}
                    issued_at = time.monotonic()
                    observation_deadline = issued_at + wait_ms / 1000 - 1
                    if outer_timeout:
                        # An outer timeout does not cancel an already-dispatched
                        # native operation. Observe its own bounded completion.
                        arguments = {"generation": generation, "mode": "all",
                                     "conditions": conditions, "wait_ms": wait_ms}
                        code = "return await call(%s, %s, %s);" % (
                            json.dumps(worker["id"]), json.dumps("ssmcp.worker.wait_for"), json.dumps(arguments))
                        future = pool.submit(self.rpc, "tools/call", {"name": "code_mode", "arguments": {
                            "agent": {"client_id": worker["id"], "id": manager["agent_id"], "token": manager["token"]},
                            "code": code, "timeout_ms": 2000,
                        }}, modern=True)
                    else:
                        future = pool.submit(self.condition_wait, worker, manager, generation, conditions,
                                             wait_ms=wait_ms)

                    def pending_operation():
                        frontend.pump()
                        current = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
                        return next((item for item in current["manager_operations"]
                                     if item["operation_id"] not in known and item["kind"] == "wait_for"
                                     and item["state"] == "waiting" and self.semantic_targets(item["targets"]) == targets), None)

                    operation = wait_for(pending_operation, timeout=max(0, observation_deadline - time.monotonic()),
                                         description="registering the actual native wait within its observation window")
                    operation_id = operation["operation_id"]
                    self.check(operation["wait_ms"] == wait_ms,
                               "the observed operation retains the original native wait budget")
                    evidence = {"waiting": operation}
                    case["outer_timeout" if outer_timeout else "native_timeout"] = evidence
                    if not outer_timeout:
                        budget = re.escape(f"{wait_ms / 1000:.1f}s")
                        pattern = re.compile(rf"Wait #{operation_id} · ALL · Waiting · (\d+\.\d)s / {budget}")

                        def elapsed_frame():
                            shot = view.frame()
                            match = pattern.search("\n".join(shot["rows"]))
                            return (shot, float(match[1])) if match else None

                        first, elapsed = wait_for(
                            elapsed_frame, timeout=max(0, observation_deadline - time.monotonic()),
                            description="the first physical worker frame for this wait")
                        target_labels = [
                            f"Task ID {task_id}",
                            f"Worker #{claim['slot'] + 1:02} · slot {claim['slot']}, epoch {claim['epoch']}",
                            f"Pool generation {generation} · operation {operation_id}",
                            "ALL: every condition must be satisfied in the same observation.",
                        ]
                        conversation = self.conversation_operation(
                            frontend, parent, f"Wait #{operation_id} · ALL", target_labels,
                            deadline=observation_deadline)
                        evidence["conversation"] = conversation

                        def elapsed_advanced():
                            frame = elapsed_frame()
                            return frame if frame is not None and frame[1] > elapsed else None

                        second, later = wait_for(
                            elapsed_advanced, timeout=max(0, observation_deadline - time.monotonic()),
                            description="the same physical worker wait advances within its original window")
                        self.check(later > elapsed, "one real native wait has an increasing visible elapsed time")
                        targets_frame = conversation["frame"]
                        parent_frame = conversation["editable_frame"]
                        self.check(all(label in "\n".join(targets_frame["rows"])
                                           for label in target_labels),
                                   "the expanded parent wait visibly identifies its exact task, worker and operation")
                        same_card = [event for event in self.parent_events(parent)
                                     if event["id"] == conversation["event"]["id"]]
                        self.check(len(same_card) == 1 and same_card[0]["expanded"],
                                   "elapsed updates preserve the original expanded wait card")
                        evidence["event_after_elapsed"] = same_card[0]
                        self.check(f"Wait #{operation_id}" in "\n".join(parent_frame["rows"])
                                   and draft in input_text(parent_frame) and not parent_frame["hide_cursor"],
                                   "visible coordinator waits preserve the parent's editable draft")
                        evidence.update(first_frame=first, later_frame=second,
                                        parent_frame=parent_frame, targets_frame=targets_frame,
                                        observation_seconds=round(time.monotonic() - issued_at, 3))
                    result = future.result(timeout=wait_ms / 1000 + 2)
                    if outer_timeout:
                        http_status, response, _ = result
                        native = response.get("result", {})
                        error = "\n".join(item.get("text", "") for item in native.get("content", []))
                        self.check(http_status == 200 and native.get("isError") is True and "timed out" in error,
                                   "the outer caller reports its real timeout without claiming task cancellation")
                        evidence["outer_error"] = error
                    else:
                        self.check(result["operation_id"] == operation_id and result["status"] == "timed_out"
                                   and result["generation"] == generation and result["mode"] == "all"
                                   and len(result["conditions"]) == len(conditions)
                                   and all(item["state"] == "pending"
                                           and expected.items() <= item.items()
                                           for expected, item in zip(conditions, result["conditions"])),
                                   "the native condition deadline returns timed_out with both conditions still pending")
                        evidence["response"] = result

                    def finished_operation():
                        current = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
                        item = next(item for item in current["manager_operations"]
                                    if item["operation_id"] == operation_id)
                        return (current, item) if item["state"] != "waiting" else None

                    current, ended = wait_for(finished_operation, timeout=wait_ms / 1000 + 1,
                                              description="the original native observation reaches its own terminal state")
                    self.check(ended["state"] in (("timed_out", "cancelled") if outer_timeout else ("timed_out",))
                               and not any(item["state"] == "waiting" for item in current["manager_operations"])
                               and current["working"] == 1
                               and any(slot["task_id"] == task_id and slot["state"] == "working"
                                       for slot in current["slots"]),
                               "ended observation leaves no active wait and does not release the task's occupied slot")
                    terminal = "Window expired" if ended["state"] == "timed_out" else "Observation stopped"
                    final_frame = wait_for(
                        lambda: self.visible_monitor_frame(view.frame, [f"Wait #{operation_id} · ALL · {terminal}"]),
                        timeout=3, description="terminal wait state replaces the waiting title")
                    evidence.update(ended=ended, terminal_frame=final_frame)
        finally:
            view.close()
        frontend.send(b"\x01\x0b")
        frontend.until(empty_composer_ready, "parent composer after the visible wait deadlines")

    def check_composer(self, frontend, label):
        marker = "WINDOW_CHAT_中文_$literal"
        frontend.send(b"\x1b[200~" + marker.encode() + b"\x1b[201~")
        frontend.until(lambda shot: marker in input_text(shot), label)
        self.check(True, label)
        frontend.send(b"\x01\x0b")

    def preserve_parent_transcripts(self):
        """Retain the full real parent PTY, including a failed physical check."""
        preserved = []
        self.evidence_directory.mkdir(parents=True, exist_ok=True)
        for index, frontend in enumerate(self.frontends):
            path = self.evidence_directory / f"parent-{index}-{frontend.label}.pty"
            path.write_bytes(frontend.transcript)
            preserved.append({"path": str(path), "bytes": path.stat().st_size,
                              "sha256": digest(path), "rows": ROWS, "cols": COLS})
        return preserved

    def preserve_child_transcripts(self):
        """Keep real child output even when an assertion prevents screenshots."""
        preserved = []
        for directory in self.windows:
            destination = self.evidence_directory / directory.name
            destination.mkdir(mode=0o700, exist_ok=True)
            for path in sorted(directory.glob("worker-*.pty")):
                copy = destination / path.name
                copy.write_bytes(path.read_bytes())
                preserved.append({"path": str(copy), "bytes": copy.stat().st_size, "sha256": digest(copy)})
            event_path = directory / "terminal-events.jsonl"
            if event_path.is_file():
                copy = destination / event_path.name
                copy.write_bytes(event_path.read_bytes())
                preserved.append({"path": str(copy), "bytes": copy.stat().st_size, "sha256": digest(copy)})
        return preserved

    def assert_children_gone(self, directory):
        identities = []
        workers = []
        for entry in events(directory):
            if entry["kind"] == "worker":
                identities.extend([entry["identity"], entry["supervisor"]])
                workers.append(entry["identity"])
        wait_for(lambda: all(process_identity(item[0]) != item for item in identities if item),
                 timeout=15, description="owned worker and detached terminal supervisor exit")
        self.check(bool(identities), "owned worker cleanup was observed for actual processes")
        self.check(not any(entry["kind"] in ("fixture_cleanup", "fixture_error") for entry in events(directory)),
                   "worker cleanup completed before fixture fallback or error handling")
        ended = [entry for entry in events(directory) if entry["kind"] == "worker_exited"]
        exited = [entry.get("identity") for entry in ended]
        self.check(bool(workers) and len(ended) == len(workers)
                   and all(identity is not None and exited.count(identity) == 1 for identity in workers)
                   and all(entry["exit_code"] == 0 and entry["terminal_restored"] for entry in ended),
                   "every actual worker has its own normal-exit and restored-terminal receipt")

    def assert_busy_reload(self, directory, worker, manager, task_id):
        original = next(entry for entry in events(directory) if entry["kind"] == "worker")
        view = ChildView(self, directory, worker, manager)
        try:
            (directory / "reload-monitor").touch()
            frame = view.until(lambda shot: "finish or cancel worker tasks" in "\n".join(shot["rows"]),
                               "busy reload rejection in the real worker panel")
            view.save("busy-worker-reload", frame)
        finally:
            view.close()
        state = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
        self.check(state["working"] == 1 and any(slot.get("task_id") == task_id for slot in state["slots"])
                   and process_identity(original["identity"][0]) == original["identity"]
                   and len([entry for entry in events(directory) if entry["kind"] == "worker"]) == 1,
                   "Ctrl+R visibly refuses a busy pool without replacing its child or releasing the active claim")
        self.cases["busy-worker-reload"] = {"worker_client_id": worker["id"],
                                            "process_id": original["identity"][0], "frame": frame}

    def reload_worker(self, frontend, directory, workspace, parent, worker):
        original = next(entry for entry in events(directory) if entry["kind"] == "worker")
        old_generation = self.cases["parent-exit"]["pool_generation"]
        frontend.send(b"\x01\x0b/worker +1\r")
        self.pool_projection(frontend, parent,
                             lambda pool: pool["desired_total"] == 4 and pool["total"] == 4
                             and pool["offline"] == 4 and pool["working"] == 0 and pool["queued"] == 0,
                             "settled four-slot capacity before worker-only reload")
        frontend.send(b"\x1b\x01\x0bPARENT_DRAFT_SURVIVES_WORKER_RELOAD")
        frontend.until(lambda shot: "PARENT_DRAFT_SURVIVES_WORKER_RELOAD" in input_text(shot),
                       "parent draft before worker reload")
        (directory / "reload-monitor").touch()
        same_parent, replacement = self.select_pair(frontend, workspace, excluding=(worker["id"],))
        records = wait_for(lambda: ([entry for entry in events(directory) if entry["kind"] == "worker"]
                                    if len([entry for entry in events(directory) if entry["kind"] == "worker"]) == 2
                                    else None), timeout=10, description="one replacement after worker Ctrl+R")
        current = records[-1]
        self.owned.extend([current["identity"], current["supervisor"]])
        wait_for(lambda: process_identity(original["identity"][0]) != original["identity"],
                 timeout=15, description="old worker exits before replacement is used")
        self.check(same_parent["id"] == parent["id"] and frontend.process.poll() is None
                   and replacement["id"] != worker["id"]
                   and current["identity"][0] != original["identity"][0],
                   "worker Ctrl+R replaces only its child process and connection, preserving the parent")
        self.check(len([entry for entry in events(directory) if entry["kind"] == "launcher"]) == 2
                   and len([entry for entry in events(directory) if entry["kind"] == "monitor_reload"]) == 2,
                   "repeated Ctrl+R keys create one replacement after the earlier busy rejection")
        new_manager = self.join(replacement["id"], "worker-reload-observer")
        state = self.bound(replacement["id"], new_manager, "ssmcp.worker.status")["worker"]
        self.check(state["generation"] != old_generation and state["working"] == 0 and state["queued"] == 0,
                   "worker-only reload starts a distinct pool generation without carrying queued or active claims")
        self.check(state["manager"] is None and state["total"] == 4 and state["desired_total"] == 4
                   and state["offline"] == 4 and state["registered_workers"] == 0,
                   "the replacement preserves requested capacity without inheriting the old manager or workers")
        expected = list(original["argv"])
        endpoint_index = expected.index("--server")
        self.check(expected.count("--username") == 1
                   and expected[endpoint_index + 1:endpoint_index + 4]
                   == [self.origin, "--username", self.identity["username"]],
                   "the child originally selected the exact named loopback account")
        # Each window gets a newly allocated systemd scope in the same session slice.
        pattern = r"--unit=ssmcp-window-[0-9a-f]{32}\.scope"
        old_scopes = [arg for arg in expected if re.fullmatch(pattern, arg)]
        new_scopes = [arg for arg in current["argv"] if re.fullmatch(pattern, arg)]
        self.check(len(old_scopes) == len(new_scopes) == 1 and old_scopes != new_scopes,
                   "the replacement worker uses a fresh window scope")
        expected[expected.index(old_scopes[0])] = new_scopes[0]
        expected[expected.index("--worker") + 1] = "4"
        self.check(current["cwd"] == str(workspace) and current["argv"] == expected,
                   "worker-only reload pins the account and preserves literal arguments and cwd")
        self.pool_projection(frontend, parent,
                             lambda pool: pool["client_id"] == replacement["id"]
                             and pool["process_id"] == current["identity"][0],
                             "unchanged parent links the replacement worker")
        frontend.until(lambda shot: "PARENT_DRAFT_SURVIVES_WORKER_RELOAD" in input_text(shot),
                       "parent draft survives worker-only reload")
        self.cases["worker-reload"] = {"parent_client_id": parent["id"],
                                      "old_worker_client_id": worker["id"], "worker_client_id": replacement["id"],
                                      "old_process_id": original["identity"][0], "process_id": current["identity"][0],
                                      "old_generation": old_generation, "generation": state["generation"],
                                      "desired_total": state["desired_total"]}
        frontend.send(b"\x01\x0b")

    def default_and_parent_exit(self):
        frontend, directory, workspace, parent, worker, manager = self.launch("parent-exit")
        self.protocol(frontend, directory, parent, worker, manager)
        self.reload_worker(frontend, directory, workspace, parent, worker)
        self.assert_no_persisted_conversation_state("active coordination")
        frontend.request_exit()
        self.check(frontend.wait_exit() == 0, "parent exits normally after two Ctrl-C presses")
        self.assert_children_gone(directory)
        self.assert_clipboard_counts()
        self.check(termios.tcgetattr(frontend.slave) == frontend.original,
                   "parent exit restores its private terminal settings")

    def worker_close(self):
        frontend, directory, _, parent, worker, _ = self.launch("worker-close")
        (directory / "close-monitor").touch()
        self.assert_children_gone(directory)
        wait_for(lambda: all(client["id"] != worker["id"] for client in self.sessions()),
                 timeout=10, description="closed monitor removed from relay inventory")
        self.check(frontend.process.poll() is None and any(client["id"] == parent["id"] for client in self.sessions()),
                   "closing the worker window preserves the main chat connection")
        self.check_composer(frontend, "main composer still accepts input after worker window closes")
        frontend.request_exit()
        self.check(frontend.wait_exit() == 0, "main chat exits independently after monitor closure")

    def parent_reload(self):
        frontend, directory, workspace, parent, worker, _ = self.launch("parent-reload")
        original = next(entry for entry in events(directory) if entry["kind"] == "worker")
        frontend.send(b"\x01\x0b/reload\r")
        new_parent, new_worker = self.select_pair(frontend, workspace,
                                                  excluding=(parent["id"], worker["id"]))
        wait_for(lambda: process_identity(original["identity"][0]) != original["identity"],
                 timeout=15, description="original worker removed during parent reload")
        records = wait_for(lambda: ([entry for entry in events(directory) if entry["kind"] == "worker"]
                                    if len([entry for entry in events(directory) if entry["kind"] == "worker"]) == 2
                                    else None), timeout=10, description="one replacement worker after reload")
        self.owned.extend([records[-1]["identity"], records[-1]["supervisor"]])
        self.check(new_parent["id"] != parent["id"] and new_worker["id"] != worker["id"],
                   "reload replaces both connection identities")
        self.check(len([entry for entry in events(directory) if entry["kind"] == "launcher"]) == 2,
                   "reload opens exactly one replacement worker window")
        manager = self.join(new_worker["id"], "replacement-manager")
        status = self.bound(new_worker["id"], manager, "ssmcp.worker.status")
        old_generation = self.cases["parent-reload"]["pool_generation"]
        self.check(status["worker"]["generation"] != old_generation
                   and status["worker"]["working"] == 0 and status["worker"]["queued"] == 0,
                   "parent reload creates a distinct empty pool generation")
        self.cases["parent-reload"].update(old_generation=old_generation,
                                             generation=status["worker"]["generation"],
                                             replacement_worker_client_id=new_worker["id"],
                                             replacement_parent_client_id=new_parent["id"])
        self.check(status["worker"]["total"] == 3 and status["worker"]["offline"] == 3,
                   "the replacement pool starts with three unstaffed stations")
        self.coordinators[new_parent["id"]] = self.join(new_parent["id"], "replacement-coordinator")
        self.pool_projection(frontend, new_parent,
                             lambda pool: pool["client_id"] == new_worker["id"]
                             and pool["process_id"] == records[-1]["identity"][0],
                             "reloaded parent projects the replacement worker process")
        frontend.until(empty_composer_ready, "interactive parent after reload")
        self.assert_no_persisted_conversation_state("parent reload")
        frontend.request_exit()
        self.check(frontend.wait_exit() == 0, "reloaded parent exits normally")
        self.assert_children_gone(directory)

    def headless(self):
        directory = self.directory / "headless"
        directory.mkdir(mode=0o700)
        environment = self.environment("headless", directory)
        command = self.client_command(name="headless-worker", extra_args=(
            "--worker", "3", "--input", "none", "--no-watchdog"))
        process = self.spawn("headless-worker", command, cwd=directory, env=environment)
        current = wait_for(lambda: next((client for client in self.sessions()
                                        if client.get("cwd") == str(directory)), None),
                           timeout=15, description="headless worker connection")
        participant = self.join(current["id"], "headless-manager")
        status = self.bound(current["id"], participant, "ssmcp.worker.status")
        self.check(directory_chat_enabled(current) is False and status["worker"]["total"] == 3,
                   "explicit headless mode retains a direct three-slot worker pool")
        self.check(not events(directory), "headless mode does not invoke a terminal launcher")
        self.stop(process)

    def missing_terminal(self):
        directory = self.directory / "missing-terminal"
        directory.mkdir(mode=0o700)
        environment = self.environment("missing-terminal", directory, terminal=False)
        command = self.client_command(name="missing-terminal", extra_args=("--worker", "3", "--no-watchdog"))
        frontend = Frontend(self, "missing-terminal", command, environment, directory)
        self.check(frontend.wait_exit() != 0, "missing terminal produces a failing exit status")
        output = (frontend.stdout + frontend.transcript).decode(errors="replace").lower()
        self.check("terminal" in output and ("worker" in output or "window" in output),
                   "missing-terminal failure explains the worker window launch problem")
        wait_for(lambda: not any(client.get("cwd") == str(directory) for client in self.sessions()),
                 timeout=10, description="failed startup removed from the relay inventory")
        self.check(True, "failed window startup leaves no connected child")

    def daemon_dispatch(self):
        directory = self.directory / "daemon-dispatch"
        directory.mkdir(mode=0o700)
        environment = self.environment("daemon-dispatch", directory)
        # Native manager discovery uses the UID's canonical runtime and system
        # executables. Hide the bus only from this child to exercise that path.
        bwrap = shutil.which("bwrap")
        if not bwrap:
            raise RuntimeError("daemon dispatch requires bubblewrap")
        def launch(*mode):
            command = self.client_command(name="daemon-worker", extra_args=("--worker", "3", "--no-watchdog", *mode))
            return subprocess.run([
                bwrap, "--die-with-parent", "--ro-bind", "/", "/", "--dev", "/dev",
                "--tmpfs", f"/run/user/{os.geteuid()}",
                "--bind", str(self.directory), str(self.directory), "--", *command,
            ], cwd=directory, env=environment, stdin=subprocess.DEVNULL, capture_output=True, timeout=15)

        state = Path(environment["SSMCP_SESSION_HOME"])
        lock_path = state / "manage.lock"
        missing_bus = b"session budget requires the current UID's systemd user bus"
        self.check(not lock_path.exists(), "the private session root starts without a management lock")
        foreground = launch()
        self.check(foreground.returncode != 0 and missing_bus in foreground.stderr and not lock_path.exists(),
                   "foreground worker admission reaches the missing bus without entering saved-session management")
        daemon = launch("--daemon")
        self.check(daemon.returncode != 0 and missing_bus in daemon.stderr and lock_path.is_file(),
                   "daemon worker enters saved-session management before checking the missing user bus")
        # Hold the file created by the actual daemon command. Only the saved-session
        # path checks this lock before it tries to contact the user manager.
        with lock_path.open("rb") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            busy = launch("--daemon")
        self.check(busy.returncode != 0 and b"another session operation or runner is already active" in busy.stderr
                   and missing_bus not in busy.stderr,
                   "daemon worker honors the held saved-session lock before manager discovery")
        self.check(not any(state.glob("session-*")) and not any(state.glob(".prepare-*")),
                   "unavailable daemon management leaves no complete or partial saved session")
        self.check(not events(directory), "daemon dispatch never launches an interactive worker window")

    def run(self):
        self.prepare()
        self.default_and_parent_exit()
        self.worker_close()
        self.parent_reload()
        self.headless()
        self.missing_terminal()
        self.daemon_dispatch()
        self.assert_no_persisted_conversation_state("completed fixture cases")

    def cleanup(self):
        for frontend in reversed(self.frontends):
            try:
                frontend.close()
            except Exception as error:
                self.cleanup_errors.append(f"frontend cleanup: {error}")
        # Failure-path cleanup targets only PIDs captured from our private
        # launcher receipts, and checks the kernel start identity before signals.
        for directory in self.windows:
            for entry in events(directory):
                if entry["kind"] == "worker":
                    self.owned.extend([entry["identity"], entry["supervisor"]])
                elif entry["kind"] in ("detached", "launcher"):
                    # A launch may fail before the child can emit its worker
                    # receipt; its already-recorded supervisor still belongs to
                    # this fixture and must not escape failure-path cleanup.
                    self.owned.append(entry["identity"])
        for identity in self.owned:
            if identity and process_identity(identity[0]) == identity:
                try:
                    signal_owned(identity, signal.SIGTERM)
                except Exception as error:
                    self.cleanup_errors.append(f"owned process cleanup: {error}")
        deadline = time.monotonic() + 5
        while any(item and process_identity(item[0]) == item for item in self.owned) and time.monotonic() < deadline:
            time.sleep(0.02)
        for identity in self.owned:
            if identity and process_identity(identity[0]) == identity:
                signal_owned(identity, signal.SIGKILL)
        super().cleanup()
        if self.cleanup_errors:
            raise AssertionError("; ".join(self.cleanup_errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path,
                        default=os.environ.get("SSMCP_E2E_FIXTURE_BINARY"))
    parser.add_argument("--decoder", type=Path)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--evidence-dir", type=Path,
                        help="new private directory for actual child PTY, cells, geometry and PNG evidence")
    parser.add_argument("--font", type=Path)
    parser.add_argument("--font-index", type=int)
    parser.add_argument("--fallback-font", type=Path)
    parser.add_argument("--fallback-font-index", type=int)
    parser.add_argument("--source-revision", default="unrecorded")
    args = parser.parse_args()
    binary_dir = args.bin_dir.resolve()
    decoder = (args.decoder or binary_dir / "examples" / "vt_snapshot").resolve()
    evidence_directory = (args.evidence_dir or args.report.with_name(args.report.stem + "-evidence")).resolve()
    font_arguments = []
    for option in ("font", "font_index", "fallback_font", "fallback_font_index"):
        value = getattr(args, option)
        if value is not None:
            font_arguments.extend(["--" + option.replace("_", "-"), str(value)])
    report = {"schema_version": 1, "suite": "worker_windows", "status": "blocked",
              "source_revision": args.source_revision, "checks": [], "cases": {},
              "script_sha256": digest(Path(__file__)), "evidence_directory": str(evidence_directory),
              "child_evidence": [], "font_arguments": font_arguments,
              "limitations": ["Busy Ctrl+R refusal preserves a live claim; it does not exercise forced-disconnect unknown work or prove replay safety."],
              "scope": {"platform": "Linux private PTYs", "relay": "private production account/router fixture",
                        "terminal": "detached argv-preserving terminal fixture", "models_started": 0,
                        "desktop_input": False, "daemon": "dispatch only; full lifecycle: e2e_daemon.py"}}
    harness = None
    exit_code = 2
    try:
        if sys.platform != "linux" or not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            raise RuntimeError("Linux pidfd support is required for isolated process cleanup")
        required = [binary_dir / "ssmcp", decoder]
        if args.fixture_binary is None:
            raise RuntimeError("supply an already-built --fixture-binary; this test never invokes Cargo")
        fixture_binary = Path(args.fixture_binary).resolve()
        required.append(fixture_binary)
        for path in required:
            if not path.is_file() or not os.access(path, os.X_OK):
                raise RuntimeError(f"required executable is unavailable: {path}")
        for font in (args.font, args.fallback_font):
            if font is not None and not font.is_file():
                raise RuntimeError(f"requested evidence font is unavailable: {font}")
        evidence_directory.mkdir(mode=0o700, parents=True, exist_ok=False)
        report["executables"] = [{"path": str(path), "sha256": digest(path)} for path in required]
        with tempfile.TemporaryDirectory(prefix="ssmcp-worker-windows-") as temporary:
            directory = Path(temporary)
            os.chmod(directory, 0o700)
            harness = WorkerWindows(directory, binary_dir, decoder, fixture_binary,
                                    evidence_directory, font_arguments)
            report["status"] = "failed"
            try:
                harness.run()
                report["status"] = "passed"
                exit_code = 0
            finally:
                report["checks"] = harness.checks
                report["cases"] = harness.cases
                report["persistence"] = harness.persistence_checks
                report["child_evidence"] = harness.child_evidence
                if report["status"] != "passed":
                    report["diagnostics"] = {front.label: {
                        "stdout_tail": front.stdout[-4096:].decode(errors="replace"),
                        "terminal_tail": front.transcript[-4096:].decode(errors="replace"),
                        "exit_code": front.process.poll(),
                    } for front in harness.frontends}
                    report["terminal_events"] = {str(path.name): events(path) for path in harness.windows}
                try:
                    harness.cleanup()
                finally:
                    report["child_transcripts"] = harness.preserve_child_transcripts()
                    report["parent_transcripts"] = harness.preserve_parent_transcripts()
                harness.assert_no_persisted_conversation_state("after all processes exited")
                harness.assert_clipboard_counts()
                report["cleanup"] = {"errors": harness.cleanup_errors,
                                     "live_owned_processes": [item[0] for item in harness.owned
                                                               if item and process_identity(item[0]) == item]}
                if report["cleanup"]["live_owned_processes"]:
                    raise AssertionError("owned worker processes survived fixture cleanup")
    except Exception as error:
        exit_code = 2 if harness is None else 1
        report["status"] = "blocked" if harness is None else "failed"
        message = f"{type(error).__name__}: {error}"
        for secret in getattr(harness, "sensitive", []):
            message = message.replace(secret, "<fixture-secret>")
        report["error"] = message
    args.report.parent.mkdir(parents=True, exist_ok=True)
    encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    for secret in getattr(harness, "sensitive", []):
        encoded = encoded.replace(secret, "<fixture-secret>")
    args.report.write_text(encoded)
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "report": str(args.report), "error": report.get("error")}, ensure_ascii=False))
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
