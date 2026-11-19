#!/usr/bin/env python3
"""Exercise private output and inherited PTYs with two isolated Linux UIDs."""
import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import pty
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


def digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def identity(path):
    value = Path(path).lstat()
    return value.st_dev, value.st_ino, value.st_uid, stat.S_IMODE(value.st_mode)


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


def finish(process):
    try:
        output, _ = process.communicate(timeout=15)
    except subprocess.TimeoutExpired:
        # Every child below starts its own session. Only that owned group is stopped.
        os.killpg(process.pid, signal.SIGKILL)
        process.communicate()
        raise AssertionError("isolated multi-user fixture exceeded 15 seconds") from None
    if process.returncode:
        raise AssertionError(f"fixture exited {process.returncode}: {output[-8192:]}")
    return output


def output_case(binary, root, temporary, uid, **extra):
    env = environment(root, uid, {"SSMCP_TEST_OUTPUT_BASE": str(temporary), **extra})
    process = subprocess.Popen(
        [str(binary), "--exact", OUTPUT_TEST, "--ignored", "--nocapture"],
        cwd=root, env=env, user=uid, group=uid, extra_groups=(),
        start_new_session=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    return finish(process)


def display_case(binary, root, different, results):
    control_master, control_slave = pty.openpty()
    other_master, other_slave = pty.openpty()
    descriptors = (control_master, control_slave, other_master, other_slave)
    try:
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
        process = subprocess.Popen(
            [str(binary), "--exact", DISPLAY_TEST, "--ignored", "--nocapture"],
            cwd=root, env=environment(root, UID_B, {"SSMCP_TEST_DISPLAY_MODE": mode}),
            stdin=control_slave, stderr=stderr, stdout=subprocess.PIPE,
            preexec_fn=enter_user, text=True)
        finish(process)
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
            captures.append(bytes(data))
        marker = b"SSMCP_MULTI_USER_DISPLAY_OK"
        if different:
            check(results, "different controlling PTY is rejected without display writes",
                  all(marker not in data for data in captures) and not any(captures))
        else:
            check(results, "foreign-owned inherited PTY accepts independent display writes",
                  marker in captures[0] and not captures[1])
    finally:
        for descriptor in descriptors:
            os.close(descriptor)


def exercise(source, results):
    with tempfile.TemporaryDirectory(prefix="ssmcp-multi-user-") as directory:
        root = Path(directory)
        root.chmod(0o755)
        binary = root / "ssmcp-lib-tests"
        shutil.copyfile(source, binary)
        binary.chmod(0o555)
        check(results, "fixture binary copy has the source checksum", digest(binary) == digest(source))
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
        before = identity(legacy), identity(sentinel), digest(sentinel)

        output_case(binary, root, temporary, UID_A)
        first = list((temporary / f"ssmcp-{UID_A}").glob("output-*.log"))
        check(results, "first UID creates a private output artifact", len(first) == 1)
        output_case(binary, root, temporary, UID_B, SSMCP_TEST_OUTPUT_FORBIDDEN=str(first[0]))
        second = list((temporary / f"ssmcp-{UID_B}").glob("output-*.log"))
        check(results, "second UID starts despite the first user's shared legacy root", len(second) == 1)
        output_case(binary, root, temporary, UID_A, SSMCP_TEST_OUTPUT_FORBIDDEN=str(second[0]))
        check(results, "neither UID can read the other's private output", True)
        for uid, artifact in ((UID_A, first[0]), (UID_B, second[0])):
            check(results, f"UID {uid} directory and artifact retain private ownership",
                  identity(artifact.parent)[2:] == (uid, 0o700)
                  and identity(artifact)[2:] == (uid, 0o600))
        check(results, "legacy parent and existing user data are unchanged",
              (identity(legacy), identity(sentinel), digest(sentinel)) == before)

        blocked = root / "blocked"
        blocked.mkdir()
        blocked.chmod(0o1777)
        foreign = blocked / f"ssmcp-{UID_A}"
        foreign.mkdir()
        os.chown(foreign, UID_B, UID_B)
        foreign.chmod(0o700)
        before = identity(foreign)
        output_case(binary, root, blocked, UID_A, SSMCP_TEST_OUTPUT_REJECT="1")
        check(results, "precreated foreign-owned private root is rejected without permission changes",
              identity(foreign) == before and not list(foreign.iterdir()))

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
