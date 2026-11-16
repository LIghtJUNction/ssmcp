#!/usr/bin/env python3
"""Exercise the real dashboard through an owned POSIX PTY and isolated SSH/OAuth.

Requires the same installed tools as e2e.py, Git, and the already-built
examples/vt_snapshot decoder. No packages, coding harnesses, or models are
installed or invoked. Linux and macOS use the same public entry points.
"""

import argparse
import base64
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import selectors
import shlex
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

from e2e import Harness


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


class Decoder:
    """Separate vt100 process; no terminal implementation is copied into Python."""

    def __init__(self, binary):
        self.process = subprocess.Popen(
            [str(binary)], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL, start_new_session=True,
        )
        self.buffer = bytearray()

    def request(self, **value):
        self.process.stdin.write(json.dumps(value).encode() + b"\n")
        self.process.stdin.flush()
        deadline = time.monotonic() + 5
        with selectors.DefaultSelector() as selector:
            selector.register(self.process.stdout, selectors.EVENT_READ)
            while b"\n" not in self.buffer:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not selector.select(remaining):
                    raise AssertionError("VT decoder did not answer within five seconds")
                block = os.read(self.process.stdout.fileno(), 65536)
                if not block:
                    raise AssertionError("VT decoder exited before its response")
                self.buffer.extend(block)
        line, _, rest = self.buffer.partition(b"\n")
        self.buffer = bytearray(rest)
        result = json.loads(line)
        if not result.get("ok"):
            raise AssertionError(f"VT decoder rejected a fixture operation: {result}")
        return result

    def feed(self, data):
        if data:
            self.request(op="feed", base64=base64.b64encode(data).decode())

    def close(self):
        if self.process.poll() is None:
            try:
                self.request(op="quit")
                self.process.wait(timeout=2)
            except (OSError, AssertionError, subprocess.TimeoutExpired):
                self.process.kill()
                self.process.wait(timeout=2)
        self.process.stdin.close()
        self.process.stdout.close()


class Frontend:
    """A disposable physical terminal, unrelated to the user's visible PTY."""

    def __init__(self, harness, label, config, decoder, *, rows=58, cols=106):
        self.harness = harness
        self.decoder = Decoder(decoder)
        self.master, self.slave = os.openpty()
        self.original_termios = termios.tcgetattr(self.slave)
        self.transcript = bytearray()
        self.stdout = bytearray()
        self.closed = False
        self.decoder.request(op="resize", rows=rows, cols=cols)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.set_blocking(self.master, False)
        environment = harness.client_environment(label)
        git = shutil.which("git")
        if not git:
            raise RuntimeError("dashboard E2E requires an already-installed Git")
        (Path(environment["PATH"]) / "git").symlink_to(Path(git).absolute())
        environment.update(harness.safe_git_env)
        environment.update({
            "SSMCP_UI_CONFIG": str(config), "TERM": "xterm-256color",
            "NO_COLOR": "1", "COLUMNS": str(cols), "LINES": str(rows),
        })

        def own_controlling_terminal():
            # The test is single-threaded when spawning. Only this child session
            # gains this new PTY; no existing terminal/process group is changed.
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            os.tcsetpgrp(0, os.getpgrp())

        self.process = subprocess.Popen(
            [str(harness.binary_dir / "ssmcp"), "-F", str(harness.directory / "ssh_config"),
             "--name", label, "--shell", "/bin/sh", f"{harness.user}@ssmcp-e2e"],
            stdin=self.slave, stdout=subprocess.PIPE, stderr=self.slave,
            cwd=harness.workspace, env=environment, preexec_fn=own_controlling_terminal,
        )
        harness.processes.append(self.process)
        os.set_blocking(self.process.stdout.fileno(), False)
        deadline = time.monotonic() + 15
        while b"\n" not in self.stdout and time.monotonic() < deadline:
            self.pump(0.05)
            if self.process.poll() is not None:
                break
        if b"\n" not in self.stdout:
            self.close()
            raise AssertionError("PTY client did not return its private connection record")
        line, _, rest = self.stdout.partition(b"\n")
        self.stdout = bytearray(rest)
        self.credentials = json.loads(line)
        harness.sensitive.append(self.credentials["login_code"])
        self.client_id = self.credentials["client_id"]

    def pump(self, seconds=0.05):
        deadline = time.monotonic() + seconds
        output = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(self.master, selectors.EVENT_READ)
            selector.register(self.process.stdout, selectors.EVENT_READ)
            while True:
                ready = selector.select(max(0, deadline - time.monotonic()))
                for key, _ in ready:
                    try:
                        data = os.read(key.fd, 65536)
                    except OSError as error:
                        if error.errno in (errno.EIO, errno.EAGAIN):
                            continue
                        raise
                    if not data:
                        selector.unregister(key.fileobj)
                    elif key.fd == self.master:
                        output.extend(data)
                        self.transcript.extend(data)
                    else:
                        self.stdout.extend(data)
                if time.monotonic() >= deadline or not selector.get_map():
                    break
        self.decoder.feed(output)

    def snapshot(self, *, cells=False):
        self.pump()
        return self.decoder.request(op="snapshot", cells=cells)

    def until(self, predicate, description, *, timeout=8, cells=False):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            snapshot = self.snapshot(cells=cells)
            if predicate(snapshot):
                return snapshot
            if self.process.poll() is not None:
                raise AssertionError(f"client exited while waiting for {description}")
        raise AssertionError(f"timed out waiting for {description}")

    def send(self, data):
        if isinstance(data, str):
            data = data.encode()
        os.write(self.master, data)

    def resize(self, rows, cols):
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        self.decoder.request(op="resize", rows=rows, cols=cols)

    def disconnect(self):
        self.send(b"\x03")
        deadline = time.monotonic() + 8
        while self.process.poll() is None and time.monotonic() < deadline:
            self.pump(0.05)
        if self.process.poll() is None:
            raise AssertionError("Ctrl-C did not disconnect this owned frontend")
        self.pump(0.1)
        return self.snapshot(), termios.tcgetattr(self.slave)

    def close(self):
        if self.closed:
            return
        self.closed = True
        try:
            if hasattr(self, "process"):
                self.harness.stop(self.process)
        finally:
            self.decoder.close()
            os.close(self.master)
            os.close(self.slave)


def text(snapshot):
    return "\n".join(snapshot["rows"])


def region(snapshot, first_row, last_row, first_col, last_col):
    return "\n".join(
        "".join(cell["text"] for cell in row[first_col:last_col])
        for row in snapshot["cells"][first_row:last_row]
    )


QUERY_FIXTURE = r'''import json, os, select, sys, termios, time, tty
from pathlib import Path
result, release = map(Path, sys.argv[1:])
old = termios.tcgetattr(0)
tty.setraw(0)
try:
    os.write(1, b"\x1b[?1049h\x1b[2J\x1b[HINNER_ALT_SCREEN\x1b[999;999HX\x1b[H")
    deadline = time.monotonic() + 12
    while not release.exists() and time.monotonic() < deadline:
        time.sleep(0.02)
    os.write(1, b"\x1b[?1049l\x1b[2J\x1b[H\x1b[3;4H")
    answers = {}
    for name, request, final in [
        ("cpr", b"\x1b[6n", b"R"), ("size", b"\x1b[18t", b"t"),
        ("status", b"\x1b[5n", b"n"), ("device", b"\x1b[c", b"c")]:
        os.write(1, request)
        response = bytearray()
        deadline = time.monotonic() + 2
        while not response.endswith(final) and time.monotonic() < deadline:
            ready, _, _ = select.select([0], [], [], 0.1)
            if ready:
                response.extend(os.read(0, 256))
        answers[name] = response.decode("ascii")
    result.write_text(json.dumps(answers))
finally:
    termios.tcsetattr(0, termios.TCSANOW, old)
os.write(1, "\r\nDASH_QUERY_DONE 界e\u0301\r\n".encode())
'''


class Dashboard(Harness):
    def __init__(self, directory, binary_dir, decoder):
        super().__init__(directory, binary_dir)
        self.decoder = decoder.resolve()
        self.frontends = []
        self.metrics = {}
        self.last_frontend = None
        self.safe_git_env = {
            "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_SYSTEM": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_COUNT": "0",
        }

    def config(self, name, *, dashboard=True, log_format="pretty"):
        path = self.directory / f"{name}.toml"
        path.write_text(f"[ui]\ndashboard={str(dashboard).lower()}\ncolor=false\n"
                        f"log_format={json.dumps(log_format)}\ntimestamps=false\n")
        return path

    def frontend(self, label, config):
        frontend = Frontend(self, label, config, self.decoder)
        self.frontends.append(frontend)
        self.last_frontend = frontend
        self.client_id = frontend.client_id
        return frontend

    def run_job(self, command, *, timeout=10000):
        return self.call("ssmcp.exec.start", {"command": command, "timeout_ms": timeout})

    def finish_job(self, job, frontend):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            result = self.call("ssmcp.exec.wait", {"job_id": job["job_id"], "wait_ms": 100})
            frontend.pump(0.05)
            if not result.get("running"):
                self.check(result.get("exit_code") == 0, "owned fixture command has its real zero exit code")
                return result
        raise AssertionError("owned fixture job did not finish within its deadline")

    def fs(self, method, arguments):
        return self.public_call("code_mode", {"code":
            f"return await fs.{method}({json.dumps(self.client_id)}, {json.dumps(arguments)});"
        })["result"]

    def pane_size(self, frontend, expected, marker):
        frontend.send(f"printf '{marker}='; stty size\n")
        snapshot = frontend.until(lambda s: f"{marker}={expected[0]} {expected[1]}" in text(s),
                                  f"inner PTY dimensions {expected}")
        self.check(f"{marker}={expected[0]} {expected[1]}" in text(snapshot),
                   f"real child PTY reports {expected[0]} rows and {expected[1]} columns")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        git_env = {**os.environ, **self.safe_git_env}
        for args in (["init", "-q"], ["-c", "user.name=Dashboard fixture", "-c",
                     "user.email=dashboard@example.invalid", "-c", "commit.gpgSign=false",
                     "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "--allow-empty", "-m", "fixture"]):
            subprocess.run(["git", *args], cwd=self.workspace, env=git_env,
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5)
        front = self.frontend("dashboard", self.config("dashboard"))
        self.oauth_login(front.credentials)
        front.until(lambda s: s["alternate_screen"] and "Tasks · 0 running" in text(s),
                    "NO_COLOR dashboard and sidebar")
        self.check(True, "a real foreground TTY enables the dashboard even with NO_COLOR")
        front.until(lambda s: "Clean working tree" in text(s), "a real clean Git observation")
        self.check(True, "sidebar observes the owned Git repository without network access")

        def owned_head():
            result = subprocess.run(
                ["git", "--no-pager", "--no-optional-locks", "-c", "log.showSignature=false",
                 "log", "-1", "--abbrev=8", "--no-patch", "--no-show-signature",
                 "--no-decorate", "--no-notes", "--format=%h%x00%s", "HEAD", "--"],
                cwd=self.workspace, env=git_env, stdout=subprocess.PIPE,
                stderr=subprocess.PIPE, text=True, encoding="utf-8", check=True, timeout=5)
            return tuple(result.stdout.rstrip("\n").split("\0", 1))

        def sidebar(snapshot):
            return region(snapshot, 0, 57, 78, 106)

        initial_head, initial_subject = owned_head()
        initial_history = front.until(
            lambda shot: "Recent commits" in sidebar(shot)
                         and initial_head in sidebar(shot)
                         and initial_subject in sidebar(shot),
            "recent commits from the owned Git HEAD", cells=True)
        self.check(initial_head in sidebar(initial_history)
                   and initial_subject in sidebar(initial_history),
                   "sidebar recent commits match the real owned Git HEAD and subject")
        self.pane_size(front, (18, 77), "DASH_SIZE_106")
        initial = front.snapshot(cells=True)
        self.check("Tasks · 0 running" in region(initial, 0, 57, 78, 106)
                   and "Shell · 1/1 · main · F7 Next · 77×18" in region(initial, 38, 39, 0, 77),
                   "106-column physical screen has distinct 77-column left panes and 28-column sidebar")
        self.check(initial["cursor"][0] >= 39 and initial["cursor"][1] < 77,
                   "physical cursor is translated into the interactive shell pane")

        for columns, expected in ((80, (18, 80)), (160, (18, 123)), (106, (18, 77))):
            front.resize(58, columns)
            self.pane_size(front, expected, f"DASH_SIZE_{columns}_RESIZED")
            shot = front.snapshot()
            self.check(("Tasks · 0 running" in text(shot)) == (columns >= 96),
                       f"SIGWINCH at {columns} columns updates both child dimensions and sidebar visibility")

        first = self.run_job([sys.executable, "-c", "import time; time.sleep(8)"])
        second = self.run_job([sys.executable, "-c", "import time; time.sleep(8)"])
        front.until(lambda s: "Tasks · 2 running" in text(s), "two real simultaneous tasks")
        self.check(first["job_id"] != second["job_id"], "two independent tasks have different identities in the live sidebar")
        front.pump(6.1)
        shot = front.snapshot(cells=True)
        side = region(shot, 0, 57, 78, 106)
        self.check(re.search(r"[6-7](?:\.\d+)?s", side) is not None and "%" not in side,
                   "tasks silent for over six seconds show elapsed time without inventing a completion percentage")
        self.finish_job(first, front)
        self.finish_job(second, front)
        front.until(lambda s: "Tasks · 0 running" in text(s), "completed tasks leaving the active sidebar")

        # Build markers from pieces so the readable command preview cannot
        # satisfy or duplicate the stream-content assertions below.
        output = self.run_job([sys.executable, "-c",
            "import sys; print('DASH_STDOUT_' + 'ONCE', flush=True); "
            "print('DASH_STDERR_' + 'ONCE', file=sys.stderr, flush=True)"])
        self.finish_job(output, front)
        front.until(lambda s: "DASH_STDERR_ONCE" in text(s), "real stderr in the observation pane")
        shot = front.snapshot(cells=True)
        logs = region(shot, 0, 38, 0, 77)
        shell = region(shot, 39, 57, 0, 77)
        self.check(logs.count("DASH_STDOUT_ONCE") == 1 and logs.count("DASH_STDERR_ONCE") == 1
                   and "DASH_STDOUT_ONCE" not in shell and "DASH_STDERR_ONCE" not in shell,
                   "stdout and stderr appear once in logs and never overwrite the manual shell")
        self.call("ssmcp.exec.read", {"job_id": output["job_id"], "max_bytes": 4096})
        self.call("ssmcp.exec.wait", {"job_id": output["job_id"], "wait_ms": 0})
        shot = front.snapshot(cells=True)
        logs = region(shot, 0, 38, 0, 77)
        self.check(logs.count("DASH_STDOUT_ONCE") == 1 and logs.count("DASH_STDERR_ONCE") == 1,
                   "read and wait do not replay stream content in the dashboard")

        changed = self.fs("writeText", {"path": "dashboard-audit.txt", "text": "one\ntwo\n", "overwrite": False})
        self.fs("readText", {"path": "dashboard-audit.txt"})
        self.fs("writeText", {"path": "dashboard-audit.txt", "text": "one\nupdated\nthree\n",
                              "overwrite": True, "expected_sha256": changed["sha256"]})
        front.until(lambda s: "dashboard-audit.txt" in text(s) and "+2" in text(s), "actual write and diff audit")
        self.check((self.workspace / "dashboard-audit.txt").read_text() == "one\nupdated\nthree\n",
                   "visible file audit and diff correspond to a successful real write")
        changed_git = front.until(
            lambda shot: "dashboard-audit.txt" in sidebar(shot),
            "Git file observation in the sidebar", cells=True)
        self.check(True, "Git sidebar refreshes from the real changed workspace")
        self.check(owned_head() == (initial_head, initial_subject)
                   and initial_head in sidebar(changed_git)
                   and initial_subject in sidebar(changed_git),
                   "uncommitted workspace changes preserve the observed commit history")

        subprocess.run(
            ["git", "--no-pager", "-c", "user.name=Dashboard fixture",
             "-c", "user.email=dashboard@example.invalid", "-c", "commit.gpgSign=false",
             "-c", "core.hooksPath=/dev/null", "commit", "--quiet", "--allow-empty",
             "-m", "fixture history refresh"],
            cwd=self.workspace, env=git_env, stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE, check=True, timeout=5)
        next_head, next_subject = owned_head()
        refreshed_git = front.until(
            lambda shot: next_head in sidebar(shot) and next_subject in sidebar(shot)
                         and initial_head in sidebar(shot),
            "changed HEAD refreshing recent commits", cells=True)
        history_text = sidebar(refreshed_git)
        self.check(next_head != initial_head
                   and history_text.index(next_head) < history_text.index(initial_head),
                   "a real owned commit refreshes HEAD and is displayed before its parent")

        fixture = self.workspace / "query-fixture.py"
        fixture.write_text(QUERY_FIXTURE)
        query_result = self.directory / "native-query.json"
        release = self.directory / "release-inner-alt"
        front.send(shlex.join([sys.executable, str(fixture), str(query_result), str(release)]) + "\n")
        front.until(lambda s: "INNER_ALT_SCREEN" in text(s), "inner full-screen application")
        shot = front.snapshot(cells=True)
        self.check("INNER_ALT_SCREEN" in region(shot, 39, 57, 0, 77)
                   and "Tasks · 0 running" in region(shot, 0, 57, 78, 106)
                   and "INNER_ALT_SCREEN" not in region(shot, 0, 38, 0, 77),
                   "inner alternate screen, clear and extreme cursor movement cannot erase logs or sidebar")
        release.write_text("continue\n")
        front.until(lambda s: "DASH_QUERY_DONE" in text(s), "native terminal query replies")
        queries = json.loads(query_result.read_text())
        self.check(queries == {"cpr": "\x1b[3;4R", "size": "\x1b[8;18;77t",
                               "status": "\x1b[0n", "device": "\x1b[?1;2c"},
                   "CPR, size, status and device queries use virtual shell coordinates and bounded local replies")
        shot = front.snapshot(cells=True)
        wide = [cell for row in shot["cells"][39:57] for cell in row[:77] if cell["text"] == "界"]
        combined = [cell for row in shot["cells"][39:57] for cell in row[:77] if cell["text"] == "e\u0301"]
        self.check(bool(wide) and wide[0]["wide"] and bool(combined),
                   "real wide and combining Unicode remain correctly represented in terminal cells")

        history = self.run_job([sys.executable, "-c",
            "import sys; sys.stdout.write(''.join('DASH_AUDIT_%04d\\n'%i for i in range(700))); sys.stdout.flush()"])
        self.finish_job(history, front)
        front.until(lambda s: "DASH_AUDIT_0699" in text(s), "last live audit line")
        front.send(b"\x1b[17~")
        front.until(lambda s: "Logs · PgUp/PgDn" in s["rows"][-1], "F6 log focus")
        front.send(b"\x1b[5~")
        paged = front.until(lambda s: "DASH_AUDIT_0699" not in text(s)
                            and "DASH_AUDIT_" in text(s), "page-up history")
        self.check(paged["hide_cursor"], "log focus hides the shell cursor while PageUp reveals prior real output")
        front.send(b"\x1b[H")
        oldest = front.until(lambda s: any(int(n) < 300 for n in re.findall(r"DASH_AUDIT_(\d{4})", text(s))),
                             "Home at the retained history boundary")
        self.check("DASH_AUDIT_0699" not in text(oldest), "Home moves to retained audit history without rewriting raw output")
        front.send(b"\x1b[F")
        front.until(lambda s: "DASH_AUDIT_0699" in text(s), "End at the live tail")
        front.send(b"\x1b")
        front.until(lambda s: "Shell · F6 Logs" in s["rows"][-1] and not s["hide_cursor"], "Escape back to shell")
        self.check(True, "End and Escape restore live logs and the real shell cursor")
        front.send(b"\x1b[17~\x1b[5~x")
        front.until(lambda s: "Shell · F6 Logs" in s["rows"][-1]
                    and "DASH_AUDIT_0699" in text(s), "combined shortcut chunk returning to the tail")
        front.send(b"\x15")
        self.check(True, "one input chunk containing F6, PageUp and text returns shell focus to live logs")

        front.send(b"\x1b[17~")
        front.until(lambda s: "Logs · PgUp/PgDn" in s["rows"][-1], "log focus before tiny resize")
        front.resize(4, 80)
        front.until(lambda s: "compact view" in text(s) and "F6" not in text(s), "tiny terminal prioritizing shell")
        self.check(not front.snapshot()["hide_cursor"], "shrinking below log-pane height returns visible focus to the shell")
        front.resize(58, 106)
        self.pane_size(front, (18, 77), "DASH_SIZE_FINAL")
        final, restored = front.disconnect()
        self.check(not final["alternate_screen"] and not final["hide_cursor"]
                   and not final["application_cursor"] and not final["bracketed_paste"],
                   "Ctrl-C leaves the alternate screen and restores cursor and input modes")
        self.check(restored == front.original_termios, "client exit restores the owned physical terminal termios exactly")
        self.check(front.process.returncode == 0, "normal frontend disconnect completes successfully")
        self.metrics["dashboard"] = {"bytes": len(front.transcript),
            "sha256": hashlib.sha256(front.transcript).hexdigest(), "native_query_replies": queries}
        front.close()

        for label, config in (("dashboard-disabled", self.config("disabled", dashboard=False)),
                              ("dashboard-jsonl", self.config("jsonl", log_format="jsonl"))):
            fallback = self.frontend(label, config)
            fallback.send("printf 'DASH_FALLBACK_READY\\n'\n")
            deadline = time.monotonic() + 5
            while b"DASH_FALLBACK_READY" not in fallback.transcript and time.monotonic() < deadline:
                fallback.pump(0.05)
            self.check(b"DASH_FALLBACK_READY" in fallback.transcript,
                       f"{label} retains an interactive shell without the compositor")
            fallback.disconnect()
            self.check(b"\x1b[?1049h" not in fallback.transcript,
                       f"{label} never enters the dashboard alternate screen")
            if label == "dashboard-jsonl":
                self.check(b"\x1b" not in fallback.transcript,
                           "JSONL event output contains no compositor or raw PTY escape bytes")
            fallback.close()

        self.client, credentials = self.start_client("dashboard-nontty", cwd=self.workspace,
            extra_env={"SSMCP_UI_CONFIG": str(self.config("nontty")), "NO_COLOR": "1"})
        self.sensitive.append(credentials["login_code"])
        self.client_id = credentials["client_id"]
        # The previous foreground connection was explicitly disconnected; its
        # grant is revoked. Authenticate this new connection before native calls.
        self.oauth_login(credentials)
        job = self.run_job([sys.executable, "-c", "print('DASH_NONTTY_READY', flush=True)"])
        self.call("ssmcp.exec.wait", {"job_id": job["job_id"], "wait_ms": 5000})
        deadline = time.monotonic() + 5
        log = self.directory / "dashboard-nontty.log"
        while b"DASH_NONTTY_READY" not in log.read_bytes() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.stop(self.client)
        streamed = log.read_bytes()
        self.check(b"DASH_NONTTY_READY" in streamed and b"\x1b[?1049h" not in streamed,
                   "non-TTY output remains a usable stream without dashboard controls")

    def close_frontends(self):
        for frontend in reversed(self.frontends):
            frontend.close()


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("dashboard E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--decoder", type=Path, help="defaults to BIN_DIR/examples/vt_snapshot")
    parser.add_argument("--report", type=Path, help="write assertions and hashes, never credentials or raw PTY output")
    args = parser.parse_args()
    decoder = args.decoder or args.bin_dir / "examples/vt_snapshot"
    if not decoder.is_file():
        parser.error(f"missing {decoder}; build the existing vt_snapshot example first")
    started = time.monotonic()
    report = {"status": "failed", "platform": platform.system(),
        "transport": "isolated OpenSSH public-key + OAuth + owned physical PTY + independent vt100 decoder",
        "scope": "real dashboard behavior; no installed coding harness or model is invoked",
        "binaries": {name: sha256(path) for name, path in (
            ("ssmcp", args.bin_dir / "ssmcp"), ("ssmcpd", args.bin_dir / "ssmcpd"), ("vt_snapshot", decoder))},
        "sources": {str(path.name): sha256(path) for path in (Path(__file__), Path(__file__).with_name("e2e.py"))}}
    with tempfile.TemporaryDirectory(prefix="ssmcp-dashboard-e2e-") as directory:
        harness = Dashboard(Path(directory), args.bin_dir, decoder)
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
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(f"\nPassed {len(report['checks'])} dashboard checks in {report['duration_seconds']}s", flush=True)


if __name__ == "__main__":
    main()
