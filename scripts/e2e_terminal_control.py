#!/usr/bin/env python3
"""Verify multiple real PTYs, background jobs and controller attribution.

Uses e2e_dashboard's disposable physical PTY and isolated account/OAuth relay.
No existing terminal, account, desktop or model is controlled. Optional PNGs
render the independently decoded live PTY cells; they are not desktop captures.
Pillow is needed only when --preview-dir is supplied. Nothing is installed.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import shlex
import signal
import subprocess
import sys
import tempfile
import time

from e2e_dashboard import Dashboard, Frontend, sha256, text

CONTROLLER = {"name": "acceptance fixture", "model": "fixture-model"}


def render(snapshot, path, label):
    from PIL import Image, ImageDraw, ImageFont

    font = None
    for candidate in ("DejaVuSansMono.ttf", "LiberationMono-Regular.ttf",
                      "/usr/share/fonts/TTF/DejaVuSansMono.ttf"):
        try:
            font = ImageFont.truetype(candidate, 16)
            break
        except OSError:
            pass
    if font is None:
        raise RuntimeError("PNG preview needs an already-installed monospace font")
    rows, columns = snapshot["size"]
    cell = font.getlength("M")
    line, margin, top = 22, 32, 76
    canvas = Image.new("RGB", (round(columns * cell + margin * 2), rows * line + top + 68), "#0f1115")
    drawing = ImageDraw.Draw(canvas)
    drawing.text((margin, 20), f"SSMCP / LIVE PTY / {label}", font=font, fill="#a7afb9")
    for index, row in enumerate(snapshot["rows"]):
        drawing.text((margin, top + index * line), row, font=font, fill="#d9dde4")
    if not snapshot["hide_cursor"]:
        row, column = snapshot["cursor"]
        x, y = margin + column * cell, top + row * line + line - 3
        drawing.rectangle((x, y, x + cell - 1, y + 1), fill="#d9dde4")
    drawing.text((margin, top + rows * line + 20),
                 "Rendered live PTY cells | fixture-model is test metadata | no model invoked",
                 font=font, fill="#808894")
    path.parent.mkdir(parents=True, exist_ok=True)
    canvas.save(path)


PASTE = b"\x1b[200~literal \x1b[18~ \x1b[17~\x1b[201~"
INPUT_FIXTURE = r'''import json, os, select, sys, termios, time, tty
from pathlib import Path
target, wanted = Path(sys.argv[1]), int(sys.argv[2])
old = termios.tcgetattr(0)
data = bytearray()
tty.setraw(0)
try:
    os.write(1, b"\x1b[?2004hPASTE_READY\r\n")
    deadline = time.monotonic() + 8
    while len(data) < wanted and time.monotonic() < deadline:
        if select.select([0], [], [], .1)[0]:
            data.extend(os.read(0, wanted - len(data)))
    target.write_text(json.dumps({"hex":data.hex()}))
finally:
    os.write(1, b"\x1b[?2004l")
    termios.tcsetattr(0, termios.TCSANOW, old)
os.write(1, b"\r\nPASTE_DONE\r\n")
'''
QUERY_FIXTURE = r'''import json, os, select, sys, termios, time, tty
from pathlib import Path
ready, release, result = map(Path, sys.argv[1:])
old = termios.tcgetattr(0)
tty.setraw(0)
try:
    ready.write_text("ready")
    deadline = time.monotonic() + 10
    while not release.exists() and time.monotonic() < deadline:
        time.sleep(.02)
    os.write(1, b"\x1b[3;4H\x1b[6n")
    answer = bytearray()
    deadline = time.monotonic() + 3
    while not answer.endswith(b"R") and time.monotonic() < deadline:
        if select.select([0], [], [], .1)[0]:
            answer.extend(os.read(0,256))
    result.write_text(json.dumps({"answer":answer.decode("ascii")}))
finally:
    termios.tcsetattr(0, termios.TCSANOW, old)
os.write(1, b"\r\nBACKGROUND_QUERY_DONE\r\n")
'''


class TerminalControl(Dashboard):
    def __init__(self, directory, binary_dir, decoder, snapshots=None, previews=None):
        super().__init__(directory, binary_dir, decoder)
        self.snapshot_dir = snapshots
        self.preview_dir = previews
        self.captures = []
        self.terminal_outputs = {}

    def native(self, name, arguments=None, *, reported=True):
        options = {"code": "return await call(%s,%s,%s);" % (
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments or {}))}
        if reported:
            options["controller"] = CONTROLLER
        result = self.public_call("code_mode", options)["result"]
        if result.get("isError"):
            raise AssertionError(f"native {name} failed: {result}")
        return result["structuredContent"]

    def read_terminal(self, terminal_id):
        cursor, previous = self.terminal_outputs.get(terminal_id, (0, ""))
        result = self.native("terminal.read", {
            "terminal_id": terminal_id, "cursor": cursor, "max_bytes": 32768, "wait_ms": 0,
        })
        if (result.get("terminal_id") != terminal_id or result.get("truncated")
                or result.get("spilled") or result.get("output_error")):
            raise AssertionError(f"small PTY fixture lost output for {terminal_id}: {result}")
        data = base64.b64decode(result["data_base64"], validate=True)
        if (result["next_cursor"] != cursor + len(data)
                or result["text"] != data.decode("utf-8", errors="replace")):
            raise AssertionError(f"PTY byte/cursor receipt disagrees for {terminal_id}: {result}")
        output = previous + result["text"]
        self.terminal_outputs[terminal_id] = (result["next_cursor"], output)
        return output

    def wait_terminal(self, front, terminal_id, marker, description):
        deadline = time.monotonic() + 8
        output = ""
        while time.monotonic() < deadline:
            output = self.read_terminal(terminal_id)
            if marker in output:
                return output
            front.pump(.05)
        raise AssertionError(f"timed out waiting for {description} in {terminal_id}: {output[-2000:]!r}")

    def capture(self, front, label):
        snapshot = front.snapshot()
        encoded = json.dumps(snapshot, ensure_ascii=False, indent=2) + "\n"
        self.check(not any(secret and secret in encoded for secret in [self.secret, *self.sensitive]),
                   f"{label} live terminal snapshot contains no authentication credentials")
        record = {"label": label, "source": "decoded real physical PTY bytes",
                  "snapshot_sha256": hashlib.sha256(encoded.encode()).hexdigest(),
                  "rows": snapshot["size"][0], "columns": snapshot["size"][1]}
        if self.snapshot_dir:
            path = self.snapshot_dir / f"terminal-{label}.json"
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(encoded)
            record["snapshot"] = path.name
        if self.preview_dir:
            path = self.preview_dir / f"terminal-{label}.png"
            render(snapshot, path, label)
            record.update(preview=path.name, preview_sha256=sha256(path))
        self.captures.append(record)
        return snapshot

    def wait_file(self, front, path, description):
        deadline = time.monotonic() + 10
        while not path.is_file() and time.monotonic() < deadline:
            front.pump(.05)
        self.check(path.is_file(), description)
        return path

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        git_env = {**os.environ, **self.safe_git_env}
        for args in (["init", "-q"], ["-c", "user.name=Acceptance fixture", "-c",
                     "user.email=fixture@example.invalid", "-c", "commit.gpgSign=false",
                     "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "--allow-empty", "-m", "terminal fixture"]):
            subprocess.run(["git", *args], cwd=self.workspace, env=git_env, check=True,
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, timeout=5)
        front = Frontend(self, "terminal-control", self.config("terminal-control"), self.decoder, rows=46, cols=144)
        self.frontends.append(front)
        self.last_frontend = front
        self.client_id = front.client_id
        self.oauth_login(front.credentials)
        front.until(lambda s: "CODE_MODE" in text(s), "code-mode audit dashboard")

        build = self.native("terminal.open", {"name": "build"})["terminal_id"]
        logs = self.native("terminal.open", {"name": "logs"})["terminal_id"]
        inventory = self.native("terminal.list")
        self.check(len(inventory["terminals"]) == 3 and inventory["selected"] == "main",
                   "three independent shells are registered without selecting background terminals")
        headings = {"main": "MAIN_WORKSPACE", build: "BUILD_WORKSPACE", logs: "LOGS_BACKGROUND"}
        for terminal, heading in headings.items():
            prefix, suffix = heading.rsplit("_", 1)
            self.native("terminal.write", {"terminal_id": terminal,
                "data": f"printf '\\033[2J\\033[H{prefix}_%s\\nIndependent shell, process and output cursor\\n' {suffix}\n"})
        for terminal, heading in headings.items():
            output = self.wait_terminal(front, terminal, heading, "independent shell output")
            self.check(all(other not in output for other in headings.values() if other != heading),
                       f"{terminal} keeps its own output and cursor without another PTY's bytes")
        self.check(self.native("terminal.list")["selected"] == "main",
                   "background PTY output does not steal the selected terminal")

        release = self.workspace / "release-jobs"
        background = "import pathlib,time; print('background job running',flush=True)\nwhile not pathlib.Path('release-jobs').exists(): time.sleep(.05)"
        jobs = [self.native("ssmcp.exec.start", {"command": [sys.executable, "-c", background], "timeout_ms": 120000}) for _ in range(2)]
        front.until(lambda s: "Tasks · 2 running" in text(s), "simultaneous background jobs")
        self.check(jobs[0]["job_id"] != jobs[1]["job_id"], "two real background commands remain independently addressable")

        selected = self.native("terminal.select", {"terminal_id": build})
        self.check(selected["terminal_id"] == build and selected["selected"],
                   "explicit terminal.select acknowledges the build shell")
        written = self.native("terminal.write", {"terminal_id": build, "data": "printf '__NATIVE_%s__\\n' BUILD\n"})
        self.check(written["terminal_id"] == build and written["written"] > 0,
                   "explicit terminal.write acknowledges bytes for the build shell")
        self.wait_terminal(front, build, "__NATIVE_BUILD__", "explicit input reaching build")
        self.check("__NATIVE_BUILD__" not in self.read_terminal("main"),
                   "explicit build input never reaches the main shell")
        self.check(self.native("terminal.list")["selected"] == build,
                   "terminal.select and terminal.list agree on the selected shell")

        fixture = self.workspace / "paste-reader.py"
        fixture.write_text(INPUT_FIXTURE)
        captured = self.workspace / "paste.json"
        self.native("terminal.write", {"terminal_id": build,
            "data": shlex.join([sys.executable, str(fixture), str(captured), str(len(PASTE))]) + "\n"})
        self.wait_terminal(front, build, "PASTE_READY", "raw paste reader")
        # The visible filter is an input-order barrier. Its preceding physical
        # paste differs from the native payload, so accidental forwarding fails
        # the child's exact-byte assertion even if only the paste is forwarded.
        front.send(PASTE.replace(b"literal", b"ui-only") + b"/LOCAL_UI_ONLY")
        front.until(lambda s: "/ LOCAL_UI_ONLY_" in s["rows"][-1], "physical input stays in audit navigation")
        written = self.native("terminal.write", {"terminal_id": build, "data": PASTE.decode("ascii")})
        self.check(written["terminal_id"] == build and written["written"] == len(PASTE),
                   "native terminal.write reports the exact literal payload length")
        self.wait_file(front, captured, "raw reader records the native literal payload")
        self.check(json.loads(captured.read_text())["hex"] == PASTE.hex(),
                   "native paste reaches the real child byte for byte; physical UI input never reaches the hidden PTY")
        self.check(self.native("terminal.list")["selected"] == build, "physical pasted F7 does not switch terminals")
        front.send("\x1b")
        front.until(lambda s: "/ LOCAL_UI_ONLY_" not in s["rows"][-1] and "CODE_MODE" in text(s),
                    "Escape closes the audit filter")
        self.wait_terminal(front, build, "PASTE_DONE", "literal paste completion")

        front.send("\x1b[18~")
        front.until(lambda s: "Copy mode" in s["rows"][-1], "F7 enters Copy mode")
        self.check(self.native("terminal.list")["selected"] == build,
                   "F7 freezes the audit display without switching the selected PTY")
        front.send("\x1b[18~")
        front.until(lambda s: "Copy mode" not in s["rows"][-1] and "CODE_MODE" in text(s),
                    "F7 returns from Copy mode to the audit dashboard")

        fixture = self.workspace / "background-query.py"
        fixture.write_text(QUERY_FIXTURE)
        ready, resume, result = (self.workspace / name for name in ("query-ready", "query-release", "query-result.json"))
        self.native("terminal.write", {"terminal_id": logs,
            "data": shlex.join([sys.executable, str(fixture), str(ready), str(resume), str(result)]) + "\n"})
        self.wait_file(front, ready, "background terminal reaches its own raw query reader")
        resume.write_text("continue")
        self.wait_file(front, result, "background terminal receives a device reply while build remains selected")
        self.check(json.loads(result.read_text())["answer"] == "\x1b[3;4R",
                   "background cursor query returns that terminal's virtual coordinates")
        self.check(self.native("terminal.list")["selected"] == build, "device replies do not change selected terminal")

        self.native("terminal.status", reported=False)
        front.until(lambda s: "Model unknown" in text(s), "truthful unknown model when caller omits it")
        self.check(True, "missing model metadata displays Model unknown instead of guessing")
        self.native("terminal.status")
        front.until(lambda s: "fixture-model" in text(s) and "caller-reported" in text(s), "reported fixture model and provenance")
        self.capture(front, "build")

        front.send("\x1b[17~")
        front.until(lambda _s: self.ui("/display/focus") == "logs", "F6 focuses logs")
        front.send("\x1b[17~")
        front.until(lambda _s: self.ui("/display/focus") == "audit", "F6 returns to code-mode audit")
        self.check(self.native("terminal.list")["selected"] == build,
                   "F6 switches log/audit focus without moving the selected PTY")

        self.native("terminal.write", {"data": "printf '__LEGACY_%s__\\n' MAIN\n"})
        self.check(self.native("terminal.list")["selected"] == build,
                   "legacy calls without terminal_id target main without moving local focus")
        selected = self.native("terminal.select", {"terminal_id": logs})
        self.check(selected["terminal_id"] == logs and selected["selected"],
                   "explicit selection reaches the retained background terminal")
        self.wait_terminal(front, logs, "BACKGROUND_QUERY_DONE", "retained background output")
        self.native("terminal.status", {"terminal_id": logs})
        self.capture(front, "logs")
        closed = self.native("terminal.close", {"terminal_id": logs})
        inventory = self.native("terminal.list")
        self.check(closed["closed"] and closed["selected"] == "main"
                   and inventory["selected"] == "main" and len(inventory["terminals"]) == 2,
                   "closing the selected terminal restores main and removes only that PTY")
        main_output = self.wait_terminal(front, "main", "__LEGACY_MAIN__", "default writes retained in main")
        self.check("MAIN_WORKSPACE" in main_output and "__LEGACY_MAIN__" not in self.read_terminal(build),
                   "closing selected terminal preserves main output; calls without terminal_id target only main")
        self.capture(front, "main")

        exited = self.native("terminal.open", {"name": "exited", "select": True})["terminal_id"]
        self.native("terminal.write", {"terminal_id": exited, "data": "exit\n"})
        deadline = time.monotonic() + 5
        while self.native("terminal.status", {"terminal_id": exited})["running"]:
            if time.monotonic() >= deadline:
                raise AssertionError("selected fixture shell did not exit")
            front.pump(.05)
        dropped = "printf '__DROPPED_%s__\\n' INPUT\n"
        try:
            self.native("terminal.write", {"terminal_id": exited, "data": dropped})
        except AssertionError as error:
            self.check("terminal has exited" in str(error), "writes to an exited PTY fail explicitly")
        else:
            raise AssertionError("exited PTY unexpectedly accepted input")
        front.send(dropped + "\x1b[18~")
        front.until(lambda s: "Copy mode" in s["rows"][-1], "F7 remains usable after the selected shell exits")
        self.check(self.native("terminal.list")["selected"] == exited,
                   "Copy mode does not redirect failed input from an exited PTY")
        front.send("\x1b[18~")
        front.until(lambda s: "Copy mode" not in s["rows"][-1], "leaving Copy mode after shell exit")
        selected = self.native("terminal.select", {"terminal_id": "main"})
        self.check(selected["terminal_id"] == "main" and selected["selected"],
                   "main remains explicitly selectable after another shell exits")
        self.native("terminal.write", {"terminal_id": "main", "data": "printf '__R%s__\\n' ECOVERED\n"})
        main_output = self.wait_terminal(front, "main", "__RECOVERED__", "main shell remains writable")
        self.check("__DROPPED_INPUT__" not in main_output,
                   "exited PTY writes and local audit input are never replayed to main")
        self.native("terminal.close", {"terminal_id": exited})

        release.write_text("finish")
        for job in jobs:
            done = self.native("ssmcp.exec.wait", {"job_id": job["job_id"], "wait_ms": 5000})
            self.check(not done["running"] and done["exit_code"] == 0,
                       "background command survives code_mode calls and exits successfully when released")
        final, restored = front.disconnect()
        self.check(not final["alternate_screen"] and not final["hide_cursor"] and restored == front.original_termios,
                   "disconnect restores the owned physical terminal and its input mode")


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("terminal control acceptance interrupted; closing owned processes")
    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--decoder", type=Path)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--snapshot-dir", type=Path)
    parser.add_argument("--preview-dir", type=Path)
    args = parser.parse_args()
    binary_dir = args.bin_dir.resolve()
    decoder = args.decoder or binary_dir / "examples/vt_snapshot"
    started = time.monotonic()
    report = {"status": "failed", "controller_fixture": CONTROLLER,
              "transport": "isolated account relay + OAuth + owned physical PTY + independent vt100 decoder",
              "preview_kind": "rendered live PTY cells, not a system desktop screenshot",
              "model_invoked": False,
              "binaries": {name: sha256(path) for name, path in (("ssmcp", binary_dir / "ssmcp"), ("ssmcpd", binary_dir / "ssmcpd"), ("vt_snapshot", decoder))}}
    with tempfile.TemporaryDirectory(prefix="ssmcp-terminal-control-") as directory:
        harness = TerminalControl(Path(directory), binary_dir, decoder, args.snapshot_dir, args.preview_dir)
        try:
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            description = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    description = description.replace(secret, "[redacted]")
            report["error"] = description
            harness.diagnostics()
            raise
        finally:
            harness.close_frontends()
            harness.cleanup()
            report.update(checks=harness.checks, captures=harness.captures,
                          script_sha256=sha256(Path(__file__)),
                          duration_seconds=round(time.monotonic() - started, 3))
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(f"Passed {len(report['checks'])} terminal-control checks in {report['duration_seconds']}s", flush=True)


if __name__ == "__main__":
    main()
