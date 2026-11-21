#!/usr/bin/env python3
"""Verify multiple real PTYs, background jobs and controller attribution.

Uses e2e_dashboard's disposable physical PTY and isolated SSH/OAuth services.
No existing terminal, account, desktop or model is controlled. Optional PNGs
render the independently decoded live PTY cells; they are not desktop captures.
Pillow is needed only when --preview-dir is supplied. Nothing is installed.
"""
import argparse
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


def shell_text(snapshot):
    rows = snapshot["rows"]
    divider = next((i for i, row in enumerate(rows) if row.lstrip().startswith("Shell ·")), None)
    if divider is None:
        return ""
    return "\n".join(row.split("│", 1)[0] for row in rows[divider + 1:-1])


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

    def native(self, name, arguments=None, *, reported=True):
        options = {"code": "return await call(%s,%s,%s);" % (
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments or {}))}
        if reported:
            options["controller"] = CONTROLLER
        result = self.public_call("code_mode", options)["result"]
        if result.get("isError"):
            raise AssertionError(f"native {name} failed: {result}")
        return result["structuredContent"]

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
        front.until(lambda s: "1/1 · main" in text(s), "main terminal and session indicator")

        build = self.native("terminal.open", {"name": "build"})["terminal_id"]
        logs = self.native("terminal.open", {"name": "logs"})["terminal_id"]
        inventory = self.native("terminal.list")
        self.check(len(inventory["terminals"]) == 3 and inventory["selected"] == "main",
                   "three independent shells are registered without selecting background terminals")
        for terminal, heading in (("main", "MAIN_WORKSPACE"), (build, "BUILD_WORKSPACE"), (logs, "LOGS_BACKGROUND")):
            self.native("terminal.write", {"terminal_id": terminal,
                "data": f"printf '\\033[2J\\033[H{heading}\\nIndependent shell, process and output cursor\\n'\n"})
        front.until(lambda s: "MAIN_WORKSPACE" in shell_text(s), "main screen after background output")
        self.check("BUILD_WORKSPACE" not in shell_text(front.snapshot()) and "LOGS_BACKGROUND" not in shell_text(front.snapshot()),
                   "background PTY output does not overwrite the selected screen")

        release = self.workspace / "release-jobs"
        background = "import pathlib,time; print('background job running',flush=True)\nwhile not pathlib.Path('release-jobs').exists(): time.sleep(.05)"
        jobs = [self.native("ssmcp.exec.start", {"command": [sys.executable, "-c", background], "timeout_ms": 120000}) for _ in range(2)]
        front.until(lambda s: "Tasks · 2 running" in text(s), "simultaneous background jobs")
        self.check(jobs[0]["job_id"] != jobs[1]["job_id"], "two real background commands remain independently addressable")

        front.send("\x1b[18~printf '__LOCAL_%s__\\n' BUILD\n")
        front.until(lambda s: "2/3 · build" in text(s) and "__LOCAL_BUILD__" in shell_text(s), "F7 routing input to build")
        main_output = self.native("terminal.read", {"max_bytes": 32768})["text"]
        self.check("__LOCAL_BUILD__" not in main_output, "F7 sends following local input only to the newly selected shell")
        self.check(self.native("terminal.list")["selected"] == build, "local F7 and remote terminal inventory share one selection")

        fixture = self.workspace / "paste-reader.py"
        fixture.write_text(INPUT_FIXTURE)
        captured = self.workspace / "paste.json"
        front.send(shlex.join([sys.executable, str(fixture), str(captured), str(len(PASTE))]) + "\n")
        front.until(lambda s: "PASTE_READY" in shell_text(s), "raw paste reader")
        front.send(PASTE)
        front.until(lambda s: "PASTE_DONE" in shell_text(s), "literal paste completion")
        self.check(json.loads(captured.read_text())["hex"] == PASTE.hex(),
                   "F7 and F6 inside bracketed paste reach the real child byte for byte")
        self.check(self.native("terminal.list")["selected"] == build, "pasted F7 does not switch terminals")

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
        front.until(lambda s: s["rows"][-1].lstrip().startswith("Logs ·"), "F6 audit focus")
        front.send("\x1b[17~")
        front.until(lambda s: s["rows"][-1].lstrip().startswith("Shell ·"), "F6 returning to shell")
        self.check(True, "F6 audit browsing remains usable beside F7 session switching")

        self.native("terminal.write", {"data": "printf '__LEGACY_%s__\\n' MAIN\n"})
        self.check(self.native("terminal.list")["selected"] == build,
                   "legacy calls without terminal_id target main without moving local focus")
        front.send("\x1b[18~")
        front.until(lambda s: "3/3 · logs" in text(s) and "BACKGROUND_QUERY_DONE" in shell_text(s), "F7 showing retained background terminal")
        self.native("terminal.status", {"terminal_id": logs})
        self.capture(front, "logs")
        self.native("terminal.close", {"terminal_id": logs})
        front.until(lambda s: "1/2 · main" in text(s) and "__LEGACY_MAIN__" in shell_text(s), "closing selected terminal restores main")
        self.check(True, "closing selected terminal restores main with its existing output intact")
        self.capture(front, "main")

        exited = self.native("terminal.open", {"name": "exited", "select": True})["terminal_id"]
        self.native("terminal.write", {"terminal_id": exited, "data": "exit\n"})
        deadline = time.monotonic() + 5
        while self.native("terminal.status", {"terminal_id": exited})["running"]:
            if time.monotonic() >= deadline:
                raise AssertionError("selected fixture shell did not exit")
            front.pump(.05)
        front.send("printf '__DROPPED_%s__\\n' INPUT\n\x1b[18~printf '__R%s__\\n' ECOVERED\n")
        front.until(lambda s: "1/3 · main" in text(s) and "__RECOVERED__" in shell_text(s),
                    "F7 recovers local input after the selected shell exits")
        self.check("__DROPPED_INPUT__" not in self.native("terminal.read", {"max_bytes": 32768})["text"],
                   "exited PTY input failure keeps F7 working and never replays failed text to main")
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
              "transport": "isolated OpenSSH + OAuth + owned physical PTY + independent vt100 decoder",
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
