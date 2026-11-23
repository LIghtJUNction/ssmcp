#!/usr/bin/env python3
"""Exercise private output and inherited PTYs with two isolated Linux UIDs."""
import argparse
from contextlib import ExitStack
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
import re
import select
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import termios
import time


OUTPUT_TEST = "output::tests::per_user_output_fixture"
DISPLAY_TEST = "client::terminal::tests::physical_output_multi_user_fixture"
UID_A, UID_B = 65534, 65533
FRAME_PREFIX = b"SSMCP_MULTI_USER_OUTPUT "
MAX_CAPTURE = 128 * 1024


def digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def identity(path):
    value = Path(path).lstat()
    return (value.st_dev, value.st_ino, value.st_uid, value.st_gid,
            stat.S_IMODE(value.st_mode))


def check(results, name, condition):
    results.append({"name": name, "passed": bool(condition)})
    if not condition:
        raise AssertionError(name)


def environment(root, uid, extra):
    home = root / f"home-{uid}"
    if not home.exists():
        home.mkdir(mode=0o700)
        os.chown(home, uid, uid)
    return {"PATH": os.defpath, "HOME": str(home), "LANG": "C.UTF-8",
            "XDG_CONFIG_HOME": str(home / "config"),
            "XDG_STATE_HOME": str(home / "state"), **extra}


class FixtureProcess:
    """One owned child, one absolute deadline, and bounded nonblocking output."""

    def __init__(self, command, **kwargs):
        self.deadline = time.monotonic() + 15
        self.output = bytearray()
        self.pending = bytearray()
        self.frames = []
        self.eof = False
        self.closed = False
        self.process = None
        try:
            self.process = subprocess.Popen(command, stdout=subprocess.PIPE, **kwargs)
            os.set_blocking(self.process.stdout.fileno(), False)
        except BaseException:
            self.close()
            raise

    def __enter__(self):
        return self

    def __exit__(self, *_):
        self.close()

    def remaining(self):
        remaining = self.deadline - time.monotonic()
        if remaining <= 0:
            raise AssertionError("isolated multi-user fixture exceeded 15 seconds")
        return remaining

    def alive(self):
        self.remaining()
        if self.process.poll() is not None:
            raise AssertionError("fixture exited before the live-peer release barrier")

    def receive(self):
        while not self.eof:
            ready, _, _ = select.select([self.process.stdout], [], [], self.remaining())
            if not ready:
                raise AssertionError("isolated multi-user fixture exceeded 15 seconds")
            try:
                chunk = os.read(self.process.stdout.fileno(), 4096)
            except BlockingIOError:
                continue
            if not chunk:
                self.eof = True
                return
            if len(self.output) + len(chunk) > MAX_CAPTURE:
                raise AssertionError("fixture stdout/stderr exceeded the capture bound")
            self.output.extend(chunk)
            self.pending.extend(chunk)
            while b"\n" in self.pending:
                line, _, rest = self.pending.partition(b"\n")
                self.pending = bytearray(rest)
                if FRAME_PREFIX in line:
                    if not line.startswith(FRAME_PREFIX):
                        raise AssertionError("malformed output fixture frame")
                    frame = json.loads(line[len(FRAME_PREFIX):])
                    if not isinstance(frame, dict):
                        raise AssertionError("output fixture frame must be an object")
                    self.frames.append(frame)
            return

    def frame(self, phase, uid):
        while not self.frames:
            self.alive()
            if self.eof:
                raise AssertionError(f"fixture stdout ended before {phase}")
            self.receive()
        self.alive()
        if len(self.frames) != 1:
            raise AssertionError("duplicate or premature output fixture frames")
        frame = self.frames.pop(0)
        if (frame.get("phase"), frame.get("uid"), frame.get("pid")) != (
                phase, uid, self.process.pid):
            raise AssertionError(f"unexpected child identity or phase: {frame}")
        return frame

    def send(self, frame):
        self.alive()
        data = json.dumps(frame).encode() + b"\n"
        if len(data) > 1024:
            raise AssertionError("output fixture command exceeds its bound")
        self.process.stdin.write(data)
        self.process.stdin.flush()
        self.remaining()

    def finish(self, selector=None, completion=None):
        if self.process.stdin is not None:
            self.process.stdin.close()
        while not self.eof:
            self.receive()
        self.process.wait(timeout=self.remaining())
        output = self.output.decode("utf-8", errors="replace")
        if self.process.returncode:
            raise AssertionError(f"fixture exited {self.process.returncode}: {output[-8192:]}")
        if self.pending.strip() or self.frames != ([] if completion is None else [completion]):
            raise AssertionError("missing, duplicate, or unexpected output completion frame")
        if selector is not None:
            running = re.findall(r"(?m)^running \d+ tests?\r?$", output)
            summaries = [line for line in output.splitlines() if line.startswith("test result:")]
            success = r"test result: ok\. 1 passed; 0 failed; 0 ignored; 0 measured; \d+ filtered out; finished in .+"
            if running != ["running 1 test"] or len(summaries) != 1 or not re.fullmatch(success, summaries[0]):
                raise AssertionError(f"{selector} must execute exactly one passing test: {output[-8192:]}")
        return output

    def close(self):
        if self.closed:
            return
        self.closed = True
        if self.process is None:
            return
        try:
            # Every child below starts its own session; include any owned descendants.
            try:
                os.killpg(self.process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            self.process.wait(timeout=1)
        finally:
            for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
                if stream is not None:
                    stream.close()


def fixture_selectors(binary, root, results):
    with FixtureProcess(
            [str(binary), "--list", "--ignored"], cwd=root,
            env=environment(root, UID_A, {}), user=UID_A, group=UID_A, extra_groups=(),
            start_new_session=True, stdin=subprocess.DEVNULL, stderr=subprocess.STDOUT) as fixture:
        listed = fixture.finish().splitlines()
    for selector in (OUTPUT_TEST, DISPLAY_TEST):
        count = listed.count(f"{selector}: test")
        check(results, f"fixture selector {selector} exists exactly once (found {count})", count == 1)


def output_cases(binary, root, temporary, blocked, foreign, results):
    with ExitStack() as owned:
        fixtures = []
        for uid, base in ((UID_A, blocked), (UID_B, temporary)):
            extra = {"TMPDIR": str(base), "SSMCP_TEST_OUTPUT_PROTOCOL": "1"}
            if uid == UID_A:
                extra["SSMCP_TEST_OUTPUT_FOREIGN_ROOT"] = str(foreign)
            fixtures.append(owned.enter_context(FixtureProcess(
                [str(binary), "--exact", OUTPUT_TEST, "--ignored", "--nocapture"],
                cwd=root, env=environment(root, uid, extra), user=uid, group=uid,
                extra_groups=(), start_new_session=True, stdin=subprocess.PIPE,
                stderr=subprocess.STDOUT)))
        ids = []
        for fixture, uid in zip(fixtures, (UID_A, UID_B)):
            frame = fixture.frame("ready", uid)
            artifact = frame.get("artifact", {})
            identifier = artifact.get("id", "")
            check(results, f"UID {uid} reads its complete paginated in-memory artifact",
                  set(frame) == {"phase", "uid", "pid", "artifact"}
                  and isinstance(identifier, str) and re.fullmatch(r"[a-f0-9]{32}", identifier)
                  and artifact.get("storage") == "memory" and artifact.get("path") is None
                  and artifact.get("complete") is True and artifact.get("error") is None
                  and artifact.get("bytes") == 2 * 32768 + 137
                  and artifact.get("observed_bytes") == artifact.get("bytes")
                  and artifact.get("scope") == f"multi-user-output:{uid}"
                  and isinstance(artifact.get("sha256"), str)
                  and re.fullmatch(r"[a-f0-9]{64}", artifact["sha256"]))
            ids.append(identifier)
        check(results, "the two live UIDs created different actual output IDs", ids[0] != ids[1])
        for fixture in fixtures:
            fixture.alive()
        for index, fixture in enumerate(fixtures):
            fixture.send({"phase": "verify", "id": ids[index], "foreign_id": ids[1 - index]})
        for index, (fixture, uid) in enumerate(zip(fixtures, (UID_A, UID_B))):
            expected = {"phase": "verified", "uid": uid, "pid": fixture.process.pid,
                        "id": ids[index], "foreign_id": ids[1 - index]}
            check(results, f"UID {uid} rejects the live peer's actual ID through read and stat",
                  fixture.frame("verified", uid) == expected)
        # Both original stores must still be alive when the second verification finishes.
        for fixture in fixtures:
            fixture.alive()
        for index, fixture in enumerate(fixtures):
            fixture.send({"phase": "release", "id": ids[index]})
        for index, (fixture, uid) in enumerate(zip(fixtures, (UID_A, UID_B))):
            fixture.finish(OUTPUT_TEST, {"phase": "complete", "uid": uid,
                           "pid": fixture.process.pid, "id": ids[index],
                           "foreign_id": ids[1 - index]})
            check(results, f"UID {uid} completes exactly one fixture after the release barrier", True)


def display_case(binary, root, different, results):
    with ExitStack() as owned:
        control_master, control_slave = pty.openpty()
        for descriptor in (control_master, control_slave):
            owned.callback(os.close, descriptor)
        other_master, other_slave = pty.openpty()
        for descriptor in (other_master, other_slave):
            owned.callback(os.close, descriptor)
        for slave in (control_slave, other_slave):
            os.fchown(slave, UID_A, UID_A)
            os.fchmod(slave, 0o600)
        stderr = other_slave if different else control_slave
        before = fcntl.fcntl(stderr, fcntl.F_GETFL)

        def enter_user():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            os.setgroups([])
            os.setgid(UID_B)
            os.setuid(UID_B)

        mode = "different" if different else "same"
        with FixtureProcess(
            [str(binary), "--exact", DISPLAY_TEST, "--ignored", "--nocapture"],
            cwd=root, env=environment(root, UID_B, {"SSMCP_TEST_DISPLAY_MODE": mode}),
            stdin=control_slave, stderr=stderr, preexec_fn=enter_user) as fixture:
            fixture.finish(DISPLAY_TEST)
        check(results, f"{mode} PTY executes exactly one passing fixture", True)
        check(results, f"{mode} PTY preserves the parent's descriptor flags",
              fcntl.fcntl(stderr, fcntl.F_GETFL) == before)
        captures = []
        for master in (control_master, other_master):
            os.set_blocking(master, False)
            data = bytearray()
            while True:
                try:
                    chunk = os.read(master, 4096)
                except OSError as error:
                    if error.errno in (errno.EAGAIN, errno.EWOULDBLOCK, errno.EIO):
                        break
                    raise
                if not chunk:
                    break
                data.extend(chunk)
                if len(data) > MAX_CAPTURE:
                    raise AssertionError("fixture PTY output exceeded the capture bound")
            captures.append(bytes(data))
        marker = b"SSMCP_MULTI_USER_DISPLAY_OK"
        if different:
            check(results, "different controlling PTY is rejected without display writes",
                  all(marker not in data for data in captures) and not any(captures))
        else:
            check(results, "foreign-owned inherited PTY accepts independent display writes",
                  marker in captures[0] and not captures[1])


def exercise(source, results):
    with tempfile.TemporaryDirectory(prefix="ssmcp-multi-user-") as directory:
        root = Path(directory)
        root.chmod(0o755)
        binary = root / "ssmcp-lib-tests"
        shutil.copyfile(source, binary)
        binary.chmod(0o555)
        check(results, "fixture binary copy has the source checksum", digest(binary) == digest(source))
        fixture_selectors(binary, root, results)
        temporary = root / "temporary"
        temporary.mkdir()
        temporary.chmod(0o1777)
        legacy = temporary / "ssmcp"
        legacy.mkdir()
        os.chown(legacy, UID_A, UID_A)
        legacy.chmod(0o1777)
        sentinel = legacy / "keep"
        sentinel.write_bytes(b"existing user data")
        os.chown(sentinel, UID_A, UID_A)
        sentinel.chmod(0o600)
        legacy_before = identity(legacy), identity(sentinel), digest(sentinel)

        blocked = root / "blocked"
        blocked.mkdir()
        blocked.chmod(0o1777)
        foreign = blocked / f"ssmcp-{UID_A}"
        foreign.mkdir()
        os.chown(foreign, UID_B, UID_B)
        foreign.chmod(0o700)
        foreign_before = identity(foreign)
        output_cases(binary, root, temporary, blocked, foreign, results)
        check(results, "legacy parent and existing user data are unchanged",
              (identity(legacy), identity(sentinel), digest(sentinel)) == legacy_before)
        check(results, "RAM output succeeds without changing the foreign-owned legacy root",
              identity(foreign) == foreign_before and not list(foreign.iterdir()))
        check(results, "in-memory outputs create no legacy artifact files or directories",
              set(temporary.iterdir()) == {legacy} and set(blocked.iterdir()) == {foreign})

        display_case(binary, root, False, results)
        display_case(binary, root, True, results)
    check(results, "all owned temporary files and PTYs are cleaned up", not root.exists())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if sys.platform != "linux" or os.geteuid() != 0:
        parser.error("run on Linux with sudo; only isolated fixture files and numeric UIDs are used")
    binary = args.fixture_binary.resolve(strict=True)
    started = time.monotonic()
    results = []
    report = {"schema_version": 1, "fixture_sha256": digest(binary), "uids": [UID_A, UID_B],
              "tests": results, "success": False}
    try:
        exercise(binary, results)
        report["success"] = True
    except Exception as error:
        report["error"] = str(error)
    report["elapsed_seconds"] = round(time.monotonic() - started, 3)
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(json.dumps(report, indent=2))
    return 0 if report["success"] else 1


if __name__ == "__main__":
    sys.exit(main())
