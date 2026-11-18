#!/usr/bin/env python3
"""Live attach acceptance helper, called from e2e_session_service.py.

Uses an isolated fixture's real service session and fresh owned PTYs. No real
user session, installed coding harness or model account is used. A second VT
parser observes the bytes sent to the physical terminal.
"""
from __future__ import annotations

import errno
import fcntl
import html
import json
import os
from pathlib import Path
import re
import selectors
import shlex
import signal
import struct
import subprocess
import tempfile
import termios
import time
import zlib

from e2e_dashboard import Decoder, text
from e2e_input_history import png_fixture


class AttachFrontend:
    def __init__(self, binary, env, cwd, name, *, read_only=False, dashboard=True,
                 rows=44, cols=110):
        self.decoder = Decoder(Path(binary).parent / "examples" / "vt_snapshot")
        self.master, self.slave = os.openpty()
        self.original_termios = termios.tcgetattr(self.slave)
        self.transcript = bytearray()
        self.closed = False
        self.decoder.request(op="resize", rows=rows, cols=cols)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ,
                    struct.pack("HHHH", rows, cols, 0, 0))
        os.set_blocking(self.master, False)
        environment = {
            **env, "TERM": "xterm-256color", "COLUMNS": str(cols), "LINES": str(rows),
            "NO_COLOR": "1",
        }
        # An explicit local UI file keeps the acceptance independent of the
        # operator's themes and preferences. It belongs to the fixture workspace.
        mode = "chat" if dashboard else "raw"
        config = Path(cwd) / f"attach-test-ui-{mode}.toml"
        config.write_text(
            f'[ui]\ndashboard = {str(dashboard).lower()}\ncolor = false\nlog_format = "pretty"\n'
        )
        environment["SSMCP_UI_CONFIG"] = str(config)

        def own_controlling_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            os.tcsetpgrp(0, os.getpgrp())

        command = [str(binary), "attach", name]
        if read_only:
            command.append("--read-only")
        try:
            self.process = subprocess.Popen(
                command, stdin=self.slave, stdout=self.slave, stderr=self.slave,
                cwd=cwd, env=environment, preexec_fn=own_controlling_terminal,
            )
        except BaseException:
            self.decoder.close()
            os.close(self.master)
            os.close(self.slave)
            raise

    def pump(self, seconds=0.05):
        deadline = time.monotonic() + seconds
        output = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(self.master, selectors.EVENT_READ)
            while time.monotonic() < deadline:
                for key, _ in selector.select(max(0, deadline - time.monotonic())):
                    try:
                        data = os.read(key.fd, 65536)
                    except OSError as error:
                        if error.errno in (errno.EIO, errno.EAGAIN):
                            continue
                        raise
                    if not data:
                        break
                    output.extend(data)
                    self.transcript.extend(data)
                    if len(self.transcript) > 16 * 1024 * 1024:
                        raise AssertionError("attach physical transcript exceeded 16 MiB")
        self.decoder.feed(output)

    def snapshot(self):
        self.pump()
        return self.decoder.request(op="snapshot")

    def until(self, predicate, description, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            snapshot = self.snapshot()
            if predicate(snapshot):
                return snapshot
            if self.process.poll() is not None:
                raise AssertionError(f"attach exited while waiting for {description}: {text(snapshot)}")
        raise AssertionError(f"attach timed out waiting for {description}: {text(self.snapshot())}")

    def send(self, value):
        if isinstance(value, str):
            value = value.encode()
        view = memoryview(value)
        while view:
            written = os.write(self.master, view)
            view = view[written:]

    def resize(self, rows, cols):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ,
                    struct.pack("HHHH", rows, cols, 0, 0))
        self.decoder.request(op="resize", rows=rows, cols=cols)

    def wait_exit(self, timeout=5):
        deadline = time.monotonic() + timeout
        while self.process.poll() is None and time.monotonic() < deadline:
            self.pump()
        if self.process.poll() is None:
            raise AssertionError("attach frontend did not exit")
        self.pump()
        return self.process.returncode

    def detach(self):
        self.send(b"\x1d")
        if self.wait_exit() != 0:
            raise AssertionError(f"Ctrl-] detach failed: {text(self.snapshot())}")
        if termios.tcgetattr(self.slave) != self.original_termios:
            raise AssertionError("detach did not restore the original physical terminal mode")

    def close(self):
        if self.closed:
            return
        self.closed = True
        try:
            if self.process.poll() is None:
                self.process.send_signal(signal.SIGTERM)
                try:
                    self.process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(self.process.pid, signal.SIGKILL)
                    self.process.wait(timeout=3)
        finally:
            self.decoder.close()
            os.close(self.master)
            os.close(self.slave)


def render_snapshot(snapshot, output, *, browser="chromium"):
    """Render decoded cells from a real captured VT frame, not invented output."""
    output = Path(output).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    columns = snapshot["size"][1]
    content = "\n".join(snapshot["rows"])
    page = f'''<!doctype html><meta charset="utf-8"><style>
body {{ margin:0; padding:28px 34px; width:calc({columns} * 1ch + 68px);
box-sizing:border-box; background:#101214; color:#d2d7de;
font:15px/22px "DejaVu Sans Mono",monospace; }}
header {{font:500 12px system-ui;letter-spacing:.12em;color:#8ab4f8;margin-bottom:20px}}
pre {{font:inherit;white-space:pre;margin:0}}
footer {{font:12px system-ui;color:#80868b;margin-top:18px}}
</style><header>SSMCP / ATTACH · LIVE ACCEPTANCE</header>
<pre>{html.escape(content)}</pre>
<footer>Real owned PTY · Detach preserved the worker and its running command.</footer>
<script>document.body.dataset.width=Math.ceil(document.body.getBoundingClientRect().width);
document.body.dataset.height=Math.ceil(document.body.getBoundingClientRect().height);</script>'''
    with tempfile.TemporaryDirectory(prefix="ssmcp-attach-preview-") as temporary:
        root = Path(temporary)
        source = root / "preview.html"
        source.write_text(page)
        command = [browser, "--headless", "--disable-gpu", "--hide-scrollbars",
                   "--no-sandbox", f"--user-data-dir={root / 'browser'}"]
        measured = subprocess.run(command + ["--dump-dom", source.as_uri()],
                                  check=True, capture_output=True, text=True, timeout=45).stdout
        width = re.search(r'data-width="(\d+)"', measured)
        height = re.search(r'data-height="(\d+)"', measured)
        if width is None or height is None:
            raise AssertionError("browser did not measure attach screenshot")
        subprocess.run(command + [f"--screenshot={output}",
                       f"--window-size={width.group(1)},{height.group(1)}", source.as_uri()],
                       check=True, capture_output=True, timeout=45)
    return str(output)


def exercise_attach(binary, env, cwd, name, status_callback, *, artifacts_dir=None,
                    browser=None):
    """Exercise one already-connected disposable fixture session.

    status_callback is a zero-argument callable returning that session's status.
    Artifacts contain only the fixture's rendered terminal transcript, never the
    private endpoint/lease descriptors. A caller can persist artifacts_dir before
    deleting its temporary Harness directory.
    """
    binary, cwd = Path(binary).resolve(), Path(cwd).resolve()
    artifacts = Path(artifacts_dir or cwd / "attach-artifacts").resolve()
    artifacts.mkdir(parents=True, exist_ok=True)
    if not (binary.parent / "examples" / "vt_snapshot").is_file():
        raise RuntimeError("build the existing vt_snapshot example before attach acceptance")
    checks = []
    frontends = []
    records = {}

    def content(path):
        try:
            return path.read_text()
        except FileNotFoundError:
            return ""

    def check(condition, label):
        if not condition:
            raise AssertionError(label)
        checks.append(label)
        print(f"PASS {label}", flush=True)

    def open_frontend(label, read_only=False, *, dashboard=False):
        frontend = AttachFrontend(binary, env, cwd, name, read_only=read_only,
                                  dashboard=dashboard,
                                  rows=26 if read_only and not dashboard else 44,
                                  cols=80 if read_only and not dashboard else 110)
        frontends.append((label, frontend))
        return frontend

    before = status_callback()
    pid = before.get("manager_pid")
    check(isinstance(pid, int) and pid > 1 and before.get("connection_state") == "connected",
          "attach fixture is an observed connected service worker")
    proof = cwd / "attach-detached-command.txt"
    readonly_proof = cwd / "attach-readonly-command.txt"
    resize_proof = cwd / "attach-terminal-size.txt"
    dashboard_interrupt_proof = cwd / "attach-dashboard-interrupt.txt"
    before_size = cwd / "attach-size-before-observer.txt"
    after_size = cwd / "attach-size-after-observer.txt"
    for path in (proof, readonly_proof, resize_proof, before_size, after_size,
                 dashboard_interrupt_proof):
        if path.exists():
            raise AssertionError("attach fixture proof path must start absent")
    try:
        # The default chat layout owns navigation; it must never send composer
        # text to the session PTY. Exercise Escape before raw terminal input.
        chat = open_frontend("chat-navigation", read_only=True, dashboard=True)
        chat.until(lambda s: s["alternate_screen"], "read-only chat attach compositor")
        chat.send("/help\r")
        help_title = "ssmcp · keyboard and commands"
        records["chat_help"] = chat.until(
            lambda s: help_title in text(s), "attach help before standalone Escape"
        )
        chat.send(b"\x1b")
        # No following keystroke may complete or accidentally flush the prefix.
        records["chat_after_escape"] = chat.until(
            lambda s: s["alternate_screen"] and help_title not in text(s),
            "standalone Escape dismissing attach help", timeout=3,
        )
        check(chat.process.poll() is None,
              "standalone Escape dismisses attach help without another keystroke")

        # A legal ancillary PNG chunk carries bytes that are not UTF-8 and
        # resemble terminal shortcuts. Make it larger than a single stdin read.
        # The observer must keep them as paste data and never detach on its 0x03.
        payload = b"raw\x00\xff\x03\x1d\x11\x13\x1b[18~" * 400
        chunk_type = b"raWp"
        chunk = (struct.pack(">I", len(payload)) + chunk_type + payload
                 + struct.pack(">I", zlib.crc32(chunk_type + payload) & 0xFFFFFFFF))
        original = png_fixture()
        pasted_png = original[:-12] + chunk + original[-12:]
        input_directory = cwd / ".ssmcp" / "inputs"
        previous_images = set(input_directory.glob("image-*.png"))
        chat.send(b"\x1b[200~")
        for offset in range(0, len(pasted_png), 2048):
            chat.send(pasted_png[offset:offset + 2048])
            chat.pump(0.01)
        chat.send(b"\x1b[201~")
        records["chat_png"] = chat.until(
            lambda s: any(image.name in text(s) for image in
                          set(input_directory.glob("image-*.png")) - previous_images),
            "raw PNG pasted through attach",
        )
        saved_image, = set(input_directory.glob("image-*.png")) - previous_images
        check(saved_image.read_bytes() == pasted_png
              and saved_image.stat().st_mode & 0o777 == 0o600,
              "attach preserves a split raw PNG byte-for-byte in a private input file")
        check(saved_image.name in text(records["chat_png"])
              and chat.process.poll() is None,
              "pasted Ctrl-C, Ctrl-] and F7 bytes stay literal in read-only attach")
        chat.detach()
        chat_status = status_callback()
        check(chat_status.get("manager_pid") == pid
              and chat_status.get("generation") == before.get("generation")
              and chat_status.get("connection_state") == "connected",
              "chat navigation and detach preserve the live service worker")

        # Shell lifecycle checks explicitly use raw input mode. Dashboard
        # layouts consume interface input even when their name is split/audit.
        writer = open_frontend("writer")
        writer.until(lambda s: f"Attached to {name}" in text(s), "raw writer attach ready")
        writer.send(f"stty -echo; stty size > {shlex.quote(str(before_size))}\n")
        deadline = time.monotonic() + 5
        while not content(before_size) and time.monotonic() < deadline:
            writer.pump()
        initial_size = before_size.read_text()
        observer = open_frontend("observer", read_only=True)
        observer.until(lambda s: f"Attached to {name}" in text(s), "raw read-only attach ready")
        conflict = open_frontend("conflicting-writer")
        conflict_code = conflict.wait_exit()
        conflict_text = " ".join(re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", conflict.transcript.decode(errors="replace")).split())
        check(conflict_code != 0 and "another attached client owns input" in conflict_text,
              "a second writer is rejected while a read-only observer remains usable")
        observer.send(f"printf forbidden > {shlex.quote(str(readonly_proof))}\n")
        observer.pump(0.2)
        check(not readonly_proof.exists(), "read-only attach does not execute typed commands")
        writer.send(f"stty size > {shlex.quote(str(after_size))}\n")
        deadline = time.monotonic() + 5
        while not content(after_size) and time.monotonic() < deadline:
            writer.pump()
        check(after_size.read_text() == initial_size,
              "a differently-sized read-only observer does not resize the writer's PTY")
        writer.send("printf '%s%s\\n' 'SSMCP_ATTACH_' 'INPUT'; "
                    f"(sleep 2; printf completed > {shlex.quote(str(proof))}; "
                    "printf '%s%s\\n' 'SSMCP_ATTACH_' 'DETACHED_DONE') &\n")
        writer.until(lambda s: "SSMCP_ATTACH_INPUT" in text(s), "typed command result")
        check(True, "attach keyboard input reaches the existing session PTY")
        records["before_detach"] = writer.snapshot()
        writer.detach()
        check(True, "Ctrl-] detaches and restores the physical terminal mode")
        deadline = time.monotonic() + 8
        while content(proof) != "completed" and time.monotonic() < deadline:
            observer.pump()
        check(proof.exists() and proof.read_text() == "completed",
              "a command launched through attach finishes after its frontend detaches")
        observer.until(lambda s: "SSMCP_ATTACH_DETACHED_DONE" in text(s), "observer sees detached job finish")
        after = status_callback()
        before_client = (before.get("connection") or {}).get("client_id")
        after_client = (after.get("connection") or {}).get("client_id")
        check(after.get("manager_pid") == pid and after.get("connection_state") == "connected"
              and after.get("generation") == before.get("generation")
              and before_client and after_client == before_client,
              "detach preserves the same worker generation and Broker connection")
        observer.send(b"\x03")
        check(observer.wait_exit() == 0
              and termios.tcgetattr(observer.slave) == observer.original_termios,
              "Ctrl-C detaches a read-only observer and restores its terminal mode")
        replacement = open_frontend("reattached-writer")
        replacement.until(lambda s: "SSMCP_ATTACH_DETACHED_DONE" in text(s), "retained terminal output on reattach")
        check(True, "reattach reacquires input and replays retained terminal output")
        replacement.resize(38, 100)
        replacement.pump(0.2)
        replacement.send(f"stty size > {shlex.quote(str(resize_proof))}\n")
        deadline = time.monotonic() + 5
        while not content(resize_proof) and time.monotonic() < deadline:
            replacement.pump()
        rows, cols = [int(value) for value in resize_proof.read_text().split()]
        check((rows, cols) == (38, 100),
              "raw attach resize applies the full physical terminal dimensions")
        replacement.send("sleep 30\n")
        replacement.pump(0.2)
        replacement.send(b"\x03")
        replacement.send("printf '%s%s\\n' 'SSMCP_ATTACH_' 'INTERRUPT_OK'\n")
        replacement.until(lambda s: "SSMCP_ATTACH_INTERRUPT_OK" in text(s), "inner Ctrl-C interrupt", timeout=5)
        check(replacement.process.poll() is None, "Ctrl-C interrupts the inner command without disconnecting attach")

        # Start a real foreground inner process through raw input, then transfer
        # the writer lease to a dashboard frontend. Ctrl-C must reach that same
        # inner process without triggering the dashboard's own SIGINT action.
        inner_program = (
            "import signal,sys,time\n"
            "from pathlib import Path\n"
            "def interrupted(*_):\n"
            f"    Path({str(dashboard_interrupt_proof)!r}).write_text('interrupted')\n"
            "    sys.exit(0)\n"
            "signal.signal(signal.SIGINT, interrupted)\n"
            "print('SSMCP_ATTACH_DASHBOARD_READY', flush=True)\n"
            "time.sleep(30)\n"
        )
        replacement.send("python3 -u -c " + shlex.quote(inner_program) + "\n")
        replacement.until(lambda s: "SSMCP_ATTACH_DASHBOARD_READY" in text(s),
                          "foreground inner task before dashboard attach")
        replacement.detach()
        dashboard_writer = open_frontend("dashboard-writer", dashboard=True)
        dashboard_writer.until(lambda s: s["alternate_screen"], "writable dashboard attach")
        dashboard_writer.send(b"\x03")
        deadline = time.monotonic() + 5
        while content(dashboard_interrupt_proof) != "interrupted" and time.monotonic() < deadline:
            dashboard_writer.pump()
        check(content(dashboard_interrupt_proof) == "interrupted"
              and dashboard_writer.process.poll() is None,
              "dashboard writer Ctrl-C interrupts the inner task while attach stays alive")
        records["dashboard_writer_interrupt"] = dashboard_writer.snapshot()
        dashboard_writer.detach()
        replacement = open_frontend("writer-after-dashboard")
        replacement.until(lambda s: f"Attached to {name}" in text(s),
                          "raw writer after dashboard interrupt")
        replacement.resize(44, 110)
        replacement.pump(0.2)
        replacement.send("printf '%s\\n' 'Session survived detach.' 'Input, output, resize and reattach verified.'; stty echo\n")
        frame = replacement.until(lambda s: "Session survived detach." in text(s), "final acceptance frame")
        records["reattached"] = frame
        (artifacts / "attach-live-frame.json").write_text(json.dumps(frame, ensure_ascii=False, indent=2) + "\n")
        replacement.detach()
        check(status_callback().get("manager_pid") == pid, "final attach detach still leaves the service worker alive")
        result = {"checks": checks, "worker_pid": pid,
                  "snapshot": text(frame), "frame": frame,
                  "transcript": b"".join(frontend.transcript for _, frontend in frontends),
                  "frame_path": str(artifacts / "attach-live-frame.json"),
                  "transcripts": [str(artifacts / f"attach-{label}.ansi") for label, _ in frontends]}
        if browser:
            result["screenshot"] = render_snapshot(frame, artifacts / "attach-live.png", browser=browser)
        return result
    finally:
        for label, frontend in reversed(frontends):
            transcript = artifacts / f"attach-{label}.ansi"
            transcript.write_bytes(frontend.transcript)
            frontend.close()
        (artifacts / "attach-checks.json").write_text(json.dumps({"checks": checks, "frames": records}, ensure_ascii=False, indent=2) + "\n")
