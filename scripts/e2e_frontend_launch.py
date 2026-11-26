#!/usr/bin/env python3
"""Exercise a real CLI's local browser entry, pairing, persistence and process lifetime (Linux)."""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
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
    environment = {**os.environ, "HOME": str(directory), "SSMCP_SESSION_HOME": str(directory / "sessions"),
                   "SSMCP_AUTH_HOME": str(directory / "auth"), "XDG_CONFIG_HOME": str(directory / "config"),
                   "XDG_DATA_HOME": str(directory / "data"), "SSMCP_CLIENT_CONFIG": str(directory / "config.toml"),
                   "PATH": str(directory) + os.pathsep + os.environ["PATH"], "FIXTURE_HANDOFF": str(handoff)}
    for name in ["SSMCP_CONTEXT", "SSMCP_TERMUX_BROWSER_FIFO", "SSMCP_RELAY_TOKEN"]:
        environment.pop(name, None)

    def start():
        handoff.unlink(missing_ok=True)
        process = subprocess.Popen([executable, "webui"], cwd=directory, env=environment,
                                   stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL,
                                   stderr=subprocess.DEVNULL)
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
            registration = directory / "sessions/live" / instance / "overview.json"
            pid = json.loads(registration.read_text())["pid"]
            assert request(url, "launch", {"destination": target}) == (200, opened)
            assert len(list((directory / "sessions/live").glob("*/overview.json"))) == 1
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
        exercise(args.client.resolve(strict=True), Path(root))
    print("PASS: first-run form, hosted consent, exact core, idempotent launch, child cleanup, saved target and bare CLI")
