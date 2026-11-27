"""Open two native desktop windows against the same loopback account under Xvfb."""
import json
import os
import contextlib
from pathlib import Path
import subprocess
import sys
import tempfile

from Xlib import X, display
from Xlib.protocol import event

from e2e_account_relay import Harness, IDENTITY, process_running, wait_for


def run(desktop):
    desktop = desktop.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="ssmcp-desktop-windows-") as directory:
        root = Path(directory)
        test = Harness(root, desktop.with_name("ssmcp"))
        home = root / "B"
        env = dict(test.env("B"), HOME=str(home), PATH=os.defpath, SHELL="/bin/sh",
                   XDG_STATE_HOME=str(home / "state"), XDG_CACHE_HOME=str(home / "cache"),
                   XDG_DATA_HOME=str(home / "data"))
        registry = home / "sessions"
        profile = home / "auth" / "desktop-target-v1.json"
        evidence = os.environ.get("SSMCP_DESKTOP_E2E_EVIDENCE")

        def open_window(label):
            frame = home / f"{label}.json"
            proc = subprocess.Popen([desktop, "--exit-after-ms", "90000",
                                     "--first-frame-receipt", frame], cwd=home, env=env,
                                    start_new_session=True, stdin=subprocess.DEVNULL)
            test.processes.append(proc)
            wait_for(lambda: frame.exists() and frame.stat().st_size,
                     f"{label} rendered its first frame", timeout=12)
            assert json.loads(frame.read_text())["pid"] == proc.pid
            return proc

        def windows():
            found = subprocess.run(["xdotool", "search", "--onlyvisible", "--name",
                                    "^ssmcp-desktop$"], capture_output=True, text=True, timeout=3)
            assert found.returncode in (0, 1)
            return set(found.stdout.split())

        def clients():
            with test.state.lock:
                return dict(test.state.sessions)

        def registration(proc):
            for overview in registry.glob("frontends/*/live/*/overview.json"):
                try:
                    pid = json.loads(overview.read_text())["pid"]
                    status = Path(f"/proc/{pid}/status").read_text()
                except (FileNotFoundError, ProcessLookupError):
                    continue
                # The desktop's bridge thread spawns the core. PPid identifies
                # its owning desktop process regardless of the spawning thread.
                if f"PPid:\t{proc.pid}" in status.splitlines():
                    return pid, overview.parents[2]
            return None

        def diagnose():
            counts = {"desktop_processes": sum(proc.poll() is None for proc in test.processes),
                      "relay_sessions": len(clients()), "visible_windows": len(windows()),
                      "profile_exists": profile.is_file()}
            with contextlib.suppress(OSError, ValueError):
                counts["profile_matches"] = json.loads(profile.read_text()) == test.state.allow_principal
            print("Desktop fixture " + json.dumps(counts, sort_keys=True), file=sys.stderr)
            for overview in sorted(registry.glob("frontends/*/live/*/overview.json"))[:2]:
                # Query just the owned synthetic core through its read-only interface.
                selector = "live:" + overview.parent.name
                try:
                    result = subprocess.run(
                        [desktop.with_name("ssmcp"), "overview", "show", selector],
                        cwd=home, env=dict(env, SSMCP_SESSION_HOME=str(overview.parents[2])),
                        text=True, capture_output=True, timeout=4)
                    data = json.loads(result.stdout) if result.returncode == 0 else {}
                    state = data.get("state")
                    state = state if state in {"connected", "connecting", "offline", "unavailable"} else "unknown"
                    print(f"Private core observer: exit={result.returncode} state={state}", file=sys.stderr)
                except (OSError, ValueError, subprocess.TimeoutExpired) as error:
                    print(f"Private core observer: {type(error).__name__}", file=sys.stderr)
            if evidence:
                # Only the desktop's status and notice bands. Never capture the chat,
                # terminal, destination fields, or raw IPC/relay identifiers.
                from PIL import Image
                output = Path(evidence)
                output.mkdir(parents=True, exist_ok=True)
                x11 = display.Display()
                try:
                    for index, window_id in enumerate(sorted(windows())[:2], start=1):
                        window = x11.create_resource_object("window", int(window_id))
                        width = window.get_geometry().width
                        for name, x, y, w in (("phase", width - 120, 16, 104),
                                             ("notice", 24, 88, min(850, width - 48))):
                            pixels = window.get_image(x, y, w, 22, X.ZPixmap, 0xffffffff)
                            image = Image.frombytes("RGB", (w, 22), pixels.data, "raw", "BGRX")
                            image.save(output / f"window-{index}-{name}.png")
                finally:
                    x11.close()

        try:
            first = open_window("first")
            window, = windows()
            assert clients() == {} and not list(registry.glob("frontends/*/live/*/overview.json"))
            subprocess.run(["xdotool", "windowfocus", "--sync", window], check=True, timeout=3)
            subprocess.run(["xdotool", "type", "--clearmodifiers", IDENTITY["username"]],
                           check=True, timeout=3)
            subprocess.run(["xdotool", "key", "Tab", "ctrl+a"], check=True, timeout=3)
            subprocess.run(["xdotool", "type", "--clearmodifiers", test.state.origin],
                           check=True, timeout=3)
            subprocess.run(["xdotool", "key", "Return"], check=True, timeout=3)

            first_id, = wait_for(clients, "first desktop relay client", timeout=12)
            first_pid, first_root = wait_for(lambda: registration(first), "first private core")
            wait_for(lambda: profile.is_file() and json.loads(profile.read_text())
                     == test.state.allow_principal, "saved desktop destination", timeout=22)

            second = open_window("second")
            assert len(windows()) == 2
            second_id, = wait_for(lambda: clients().keys() - {first_id},
                                  "second independent relay client", timeout=12)
            second_pid, second_root = wait_for(lambda: registration(second), "second private core")
            assert first_pid != second_pid and first_root != second_root
            for private in (first_root, second_root):
                assert private.stat().st_mode & 0o077 == 0
            assert not list(registry.glob("live/*/overview.json"))

            # Ask the native window to close through the standard X11 protocol.
            x11 = display.Display()
            try:
                target = x11.create_resource_object("window", int(window))
                target.send_event(event.ClientMessage(
                    window=target, client_type=x11.intern_atom("WM_PROTOCOLS"),
                    data=(32, [x11.intern_atom("WM_DELETE_WINDOW"), X.CurrentTime, 0, 0, 0])))
                x11.sync()
            finally:
                x11.close()
            assert first.wait(timeout=8) == 0
            wait_for(lambda: not process_running(first_pid) and first_id not in clients(),
                     "first desktop core and relay session closed")
            assert second.poll() is None and process_running(second_pid)
            job = clients()[second_id].call("ssmcp.exec.start",
                                            {"command": ["/bin/pwd"], "mode": "pipes"})
            survivor = clients()[second_id]
            survivor.call("ssmcp.exec.wait", {"job_id": job["job_id"], "wait_ms": 5000})
            result = survivor.call("ssmcp.exec.read", {"job_id": job["job_id"]})
            assert result["exit_code"] == 0 and result["stdout"].strip() == str(home)
            assert not test.state.errors, test.state.errors
            print("PASS: first-run keyboard connection, two independent desktop windows, "
                  "and surviving relay session after the first window closes")
        except Exception:
            try:
                diagnose()
            except Exception as error:
                print(f"Desktop diagnostic failed: {type(error).__name__}", file=sys.stderr)
            raise
        finally:
            test.cleanup()


if __name__ == "__main__":
    run(Path(sys.argv[1]))
