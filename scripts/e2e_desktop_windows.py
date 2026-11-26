"""Open two native desktop windows against the same loopback account under Xvfb."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from Xlib import X, display
from Xlib.protocol import event

from e2e_account_relay import Harness, IDENTITY, descendants, process_running, wait_for


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
            children = set(descendants(proc.pid))
            for overview in registry.glob("frontends/*/live/*/overview.json"):
                pid = json.loads(overview.read_text())["pid"]
                if pid in children:
                    return pid, overview.parents[2]
            return None

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
            profile = home / "auth" / "desktop-target-v1.json"
            wait_for(lambda: profile.is_file() and json.loads(profile.read_text())
                     == test.state.allow_principal, "saved desktop destination")

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
        finally:
            test.cleanup()


if __name__ == "__main__":
    run(Path(sys.argv[1]))
