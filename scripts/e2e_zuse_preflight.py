#!/usr/bin/env python3
"""Exercise the real zuse worker against an owned, non-desktop IPC fixture.

The private socket only returns compositor metadata and records requests.
It never forwards input, connects to a user's compositor, or opens a display.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import socket
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[1]
HELD = "/repl return (function() for code=8,775 do if hl.is_key_down(code) then return true end end return false end)()"
NEUTRAL = {"keyboards": [{"main": True, "capsLock": False, "numLock": False, "active_layout_index": 0}]}


def run_case(binary, name, version, locked=False, held=False, dispatch_failure=False):
    requests, errors = [], []
    with tempfile.TemporaryDirectory(prefix="ssmcp-zuse-preflight-") as temporary:
        root = Path(temporary)
        root.chmod(0o700)
        env = os.environ.copy()
        for key in ("DISPLAY", "WAYLAND_SOCKET", "SWAYSOCK", "HYPRLAND_INSTANCE_SIGNATURE",
                    "ZTTY_SOCKET", "DBUS_SESSION_BUS_ADDRESS", "LD_PRELOAD", "LD_LIBRARY_PATH"):
            env.pop(key, None)
        env.update(HOME=str(root), XDG_RUNTIME_DIR=str(root), XDG_CONFIG_HOME=str(root / "config"),
                   WAYLAND_DISPLAY="ssmcp-fixture-no-display", ZUSE_BACKEND="wayland")
        listener, thread = None, None
        stopping = threading.Event()
        if version is not None:
            env["HYPRLAND_INSTANCE_SIGNATURE"] = "ssmcp-preflight"
            address = root / "hypr" / env["HYPRLAND_INSTANCE_SIGNATURE"] / ".socket.sock"
            address.parent.mkdir(parents=True)
            listener = socket.socket(socket.AF_UNIX)
            listener.bind(str(address))
            listener.listen(8)
            listener.settimeout(0.1)

            def serve():
                while not stopping.is_set():
                    try:
                        connection, _ = listener.accept()
                    except socket.timeout:
                        continue
                    except OSError:
                        if not stopping.is_set():
                            errors.append("fixture accept failed")
                        break
                    with connection:
                        connection.settimeout(0.05)
                        data = bytearray()
                        try:
                            while len(data) < 4096:
                                part = connection.recv(4096 - len(data))
                                if not part:
                                    break
                                data.extend(part)
                        except socket.timeout:
                            pass
                        command = data.decode("utf-8")
                        requests.append(command)
                        if command == "j/version":
                            response = json.dumps({"tag": version})
                        elif command == "j/devices":
                            devices = json.loads(json.dumps(NEUTRAL))
                            devices["keyboards"][0]["numLock"] = locked
                            response = json.dumps(devices)
                        elif command == HELD:
                            response = "true" if held else "false"
                        elif command.startswith("/eval hl.dispatch(") and dispatch_failure:
                            response = "fixture rejected dispatch"
                        else:
                            errors.append("unexpected request: " + command)
                            response = "fixture rejected unexpected request"
                        try:
                            connection.sendall(response.encode())
                        except OSError as error:
                            errors.append(str(error))

            thread = threading.Thread(target=serve, daemon=True)
            thread.start()
        process = subprocess.Popen([str(binary), "--internal-zuse-worker"], env=env, cwd=root,
                                   stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                   stderr=subprocess.PIPE, bufsize=0)
        pending = bytearray()

        def line():
            deadline = time.monotonic() + 15
            while b"\n" not in pending:
                remaining = deadline - time.monotonic()
                if remaining <= 0 or not select.select([process.stdout], [], [], remaining)[0]:
                    raise TimeoutError("worker reply timed out")
                chunk = os.read(process.stdout.fileno(), 4096)
                if not chunk:
                    raise RuntimeError("worker exited before its reply")
                pending.extend(chunk)
                if len(pending) > 16384:
                    raise RuntimeError("worker reply exceeded fixture budget")
            result, _, rest = pending.partition(b"\n")
            pending[:] = rest
            return bytes(result)

        try:
            assert line() == b"ssmcp-zuse-worker-v1"
            request = {"operation": "key", "arguments": {"key": "Return", "modifiers": []}}
            process.stdin.write(json.dumps(request).encode() + b"\n")
            process.stdin.flush()
            reply = json.loads(line())
            process.stdin.close()
            process.stdin = None
            code = process.wait(timeout=10)
            stderr = process.stderr.read(4096).decode("utf-8", "replace")
            assert reply["value"] is None
            if dispatch_failure:
                assert reply["cleanup_ok"] is False and code != 0, (reply, code)
                assert reply["error"].startswith("Hyprland: fixture rejected dispatch"), reply
                assert sum(command.startswith("/eval ") for command in requests) == 1, requests
            else:
                assert reply["cleanup_ok"] is True and code == 0, (reply, code, stderr)
                assert reply["error"].startswith("unsupported_state:"), reply
                assert all(command in ("j/version", "j/devices", HELD) for command in requests)
                if version is None:
                    assert requests == []
                elif version == "0.54.0":
                    assert requests == ["j/version"]
                elif locked:
                    assert requests == ["j/version", "j/devices"]
                elif held:
                    assert requests == ["j/version", "j/devices", HELD]
            assert not errors, errors
            return {"name": name, "ok": True, "reply": reply, "exit_code": code,
                    "requests": requests, "physical_input": False}
        finally:
            if process.poll() is None:
                process.kill()
                process.wait(timeout=5)
            for stream in (process.stdin, process.stdout, process.stderr):
                if stream is not None:
                    stream.close()
            stopping.set()
            if listener is not None:
                listener.close()
            if thread is not None:
                thread.join(timeout=2)
                assert not thread.is_alive(), "owned fixture thread did not exit"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    report = {"ok": False, "scope": "real worker; private compositor IPC fixture; no graphical input",
              "source": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "script_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(), "checks": []}
    cases = [
        ("missing_hyprland", None, False, False, False),
        ("older_hyprland", "0.54.0", False, False, False),
        ("num_lock_enabled", "0.55.0", True, False, False),
        ("held_keyboard_key", "0.55.0", False, True, False),
        ("unknown_dispatch_failure_stays_unconfirmed", "0.55.0", False, False, True),
    ]
    try:
        for case in cases:
            report["checks"].append(run_case(binary, *case))
        report["ok"] = True
    except Exception as error:
        report["error"] = repr(error)
    finally:
        report["binary_unchanged"] = report["binary_sha256"] == hashlib.sha256(binary.read_bytes()).hexdigest()
        report["ok"] = report["ok"] and report["binary_unchanged"]
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
