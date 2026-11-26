#!/usr/bin/env python3
"""Exercise a real CLI's local browser entry, pairing, persistence and process lifetime (Linux)."""
import argparse
import json
import os
import re
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time
from urllib.error import HTTPError
from urllib.parse import parse_qs, urlsplit
from urllib.request import Request, urlopen


def wait_for(probe, process=None):
    end = time.monotonic() + 12
    while time.monotonic() < end:
        result = probe()
        if result:
            return result
        if process and process.poll() is not None:
            raise AssertionError(f"ssmcp stopped before browser opened ({process.returncode})")
        time.sleep(0.05)
    raise AssertionError("timed out waiting for frontend")


def request(handoff, path, payload):
    parts = urlsplit(handoff)
    origin = f"{parts.scheme}://{parts.netloc}"
    req = Request(origin + "/api/gui/v1/" + path, method="POST",
                  data=json.dumps(payload).encode(),
                  headers={"Authorization": "Bearer " + parts.fragment, "Origin": origin,
                           "Content-Type": "application/json"})
    try:
        with urlopen(req, timeout=10) as response:
            return response.status, json.load(response)
    except HTTPError as error:
        return error.code, json.load(error)


def exercise(executable, directory):
    handoff = directory / "launch-url"
    opener = directory / "xdg-open"
    opener.write_text('#!/bin/sh\nprintf "%s\\n" "$1" > "$FIXTURE_HANDOFF"\n')
    opener.chmod(0o700)
    # A fresh install has no required config file. Keep fixture-generated state private.
    inherited = {key: value for key, value in os.environ.items() if not key.startswith("SSMCP_")}
    environment = {**inherited, "HOME": str(directory), "SSMCP_SESSION_HOME": str(directory / "sessions"),
                   "SSMCP_AUTH_HOME": str(directory / "auth"), "XDG_CONFIG_HOME": str(directory / "config"),
                   "XDG_DATA_HOME": str(directory / "data"), "XDG_STATE_HOME": str(directory / "state"),
                   "PATH": str(directory) + os.pathsep + os.environ["PATH"], "FIXTURE_HANDOFF": str(handoff)}

    def start():
        handoff.unlink(missing_ok=True)
        with (directory / "launch.stderr").open("ab") as diagnostics:
            process = subprocess.Popen([executable, "webui"], cwd=directory, env=environment,
                                       stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                       stderr=diagnostics)
        try:
            url = wait_for(lambda: handoff.is_file() and handoff.read_text().strip(), process)
            parts = urlsplit(url)
            assert parts.hostname == "127.0.0.1" and len(parts.fragment) == 64
            return process, url
        except BaseException:
            process.kill()
            process.wait()
            raise

    def stop(process):
        process.send_signal(signal.SIGTERM)
        try:
            assert process.wait(timeout=7) == 0
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()

    with socket.socket() as relay:
        relay.bind(("127.0.0.1", 0))
        relay.listen()
        target = f"alice@http://127.0.0.1:{relay.getsockname()[1]}"
        process, url = start()
        try:
            assert parse_qs(urlsplit(url).query) == {"setup": ["1"]}
            assert not (directory / "sessions/live").exists()
            status, _ = request(url, "launch", {"destination":"alice@ssmcp.lmm.best"})
            assert status == 400, status
            status, opened = request(url, "launch", {"destination": target})
            assert status == 200, (status, opened)
            instance = opened["session"].removeprefix("live:")
            assert len(instance) == 32
            registrations = list((directory / "sessions/gui/owners").glob(f"*/live/{instance}/overview.json"))
            assert len(registrations) == 1, "the browser must own exactly one matching private core"
            registration = registrations[0]
            assert registration.parents[2].stat().st_mode & 0o077 == 0
            assert not (directory / "sessions/live" / instance).exists()
            pid = json.loads(registration.read_text())["pid"]
            assert request(url, "launch", {"destination": target}) == (200, opened)
            assert not list((directory / "sessions/live").glob("*/overview.json"))
        finally:
            stop(process)
        wait_for(lambda: not Path(f"/proc/{pid}").exists())
        bare = subprocess.run([executable], cwd=directory, env=environment,
                              capture_output=True, timeout=3)
        assert bare.returncode and not bare.stdout and b"ssmcp username@server" in bare.stderr
        process, url = start()
        try:
            selected = parse_qs(urlsplit(url).query)["session"][0]
            assert selected.startswith("live:") and selected != opened["session"]
        finally:
            stop(process)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--client", type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="ssmcp-browser-fixture-") as root:
        directory = Path(root)
        try:
            exercise(args.client.resolve(strict=True), directory)
        except Exception:
            log = directory / "launch.stderr"
            if log.exists():
                with log.open("rb") as stream:
                    stream.seek(max(0, log.stat().st_size - 8192))
                    tail = stream.read().decode("utf-8", errors="replace")
                # Browser handoff fragments and protocol credentials stay out of CI logs.
                tail = re.sub(r"(https?://[^\s#]+)#[^\s]+", r"\1#[redacted]", tail)
                tail = re.sub(r"""(?i)(bearer\s+|(?:token|code|secret)["']?\s*[:=]\s*["']?)[^\s,"'}]+""",
                              r"\1[redacted]", tail)
                tail = re.sub(r"(?i)\b[a-f0-9]{32,}\b", "[redacted]", tail)
                print("Frontend/core stderr:\n" + tail, file=sys.stderr)
            raise
    print("PASS: first-run form, hosted consent, exact core, idempotent launch, child cleanup, saved target and bare CLI")
