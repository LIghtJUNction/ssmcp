"""Verify session.new and session.cd against a loopback relay and production gateway."""
import argparse
import fcntl
import struct
import subprocess
import termios
import threading
import json
import os
from pathlib import Path
import tempfile
import urllib.request
import uuid

from e2e_account_relay import Harness, wait_for


def check_tui(test, workspace, sub, nested, http, attach, bridge, state):
    """Drive the ordinary TUI through a controlling terminal; observe only over IPC."""
    master, slave = os.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 42, 106, 0, 0))
    environment = test.env("B")
    environment.update(TERM="xterm-256color", LINES="42", COLUMNS="106")

    def own_terminal():
        os.setsid()
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        os.tcsetpgrp(0, os.getpgrp())

    process = subprocess.Popen(
        [test.binary, test.state.allow_principal, "--name", "keyboard-primary",
         "--no-setup", "--shell", "/bin/sh", "--no-watchdog"],
        cwd=workspace, env=environment, stdin=slave, stdout=subprocess.DEVNULL,
        stderr=slave, preexec_fn=own_terminal,
    )
    test.processes.append(process)
    frames = []
    alternate = b"\x1b[?1049h"

    def drain():
        tail = b""
        try:
            while data := os.read(master, 65536):
                output = tail + data
                frames.extend([True] * output.count(alternate))
                tail = output[-(len(alternate) - 1):]
        except OSError:
            pass  # The slave terminal closed.

    reader = threading.Thread(target=drain, daemon=True)
    reader.start()
    try:
        test.connection("keyboard-primary")
        def inventory():
            return http("sessions")["sessions"]
        own = wait_for(lambda: next((s for s in inventory()
                                     if s["name"] == "keyboard-primary"), None),
                       "foreground keyboard session")
        parent = attach(own["target"], read_only=True)
        def cwd(connection):
            cap = bridge(connection, "capabilities")
            return state(connection, cap["generation"])["cwd"]
        wait_for(lambda: len(frames) >= 1 and cwd(parent) == str(workspace)
                 and not (termios.tcgetattr(slave)[3] & termios.ICANON),
                 "primary physical composer and its real runtime", timeout=20)
        os.write(master, b"/cd project\r")
        wait_for(lambda: cwd(parent) == str(sub), "directory from actual parent keyboard")
        before = {s["target"] for s in inventory()}
        frame_count = len(frames)
        os.write(master, b"/new\r")
        new = wait_for(lambda: next((s for s in inventory() if s["target"] not in before), None),
                       "new session from actual parent keyboard", timeout=20)
        child = attach(new["target"], read_only=True)
        wait_for(lambda: len(frames) > frame_count and cwd(child) == str(sub),
                 "child takes the physical alternate screen", timeout=45)
        os.write(master, b"/cd child only\r")
        wait_for(lambda: cwd(child) == str(nested), "child accepts physical keyboard input")
        assert cwd(parent) == str(sub)
        frame_count = len(frames)
        os.write(master, b"\x1d")
        wait_for(lambda: len(frames) > frame_count, "parent compositor resumes on Ctrl-]")
        os.write(master, b"/cd ..\r")
        wait_for(lambda: cwd(parent) == str(workspace), "restored parent keyboard focus")
        assert cwd(child) == str(nested)
        print("PASS: physical TUI /new switches keyboard focus to its independent session; "
              "Ctrl-] restores the parent's composer and both directory states", flush=True)
    finally:
        try:
            test.stop(process, expected=None)
        finally:
            os.close(slave)
            reader.join(timeout=1)
            os.close(master)


def run(binary):
    with tempfile.TemporaryDirectory(prefix="ssmcp-workbench-") as tmp:
        root = Path(tmp)
        test = Harness(root, binary)
        workspace = root / "B"
        sub = workspace / "project"
        nested = sub / "child only"
        nested.mkdir(parents=True)
        # Keep all private runtime files and session manifests inside the fixture.
        base_env = test.env
        def env(device):
            result = base_env(device)
            result.update(HOME=str(root / device),
                          XDG_STATE_HOME=str(root / device / "state"),
                          XDG_DATA_HOME=str(root / device / "data"),
                          XDG_CACHE_HOME=str(root / device / "cache"),
                          PATH=os.defpath, SHELL="/bin/sh")
            return result
        test.env = env
        try:
            parent = test.unsigned("primary")
            test.connection("primary")
            pairing = workspace / "auth" / "pairing.json"
            test.spawn("B", "gui", "serve", "--pairing-file", str(pairing))
            wait_for(pairing.exists, "gateway pairing")
            pair = json.loads(pairing.read_text())
            origin = pair["url"]
            def http(path, data=None):
                request = urllib.request.Request(
                    origin + "/api/gui/v1/" + path,
                    data=None if data is None else json.dumps(data).encode(),
                    headers={"Authorization": "Bearer " + pair["token"],
                             "Origin": origin, "Content-Type": "application/json"},
                )
                with urllib.request.urlopen(request, timeout=15) as response:
                    return json.load(response)
            def attach(target, read_only=False):
                entry = http("connections", {"session":target, "request_id":uuid.uuid4().hex,
                                               "read_only":read_only})
                return entry["connection_id"]
            def bridge(connection, op, **kw):
                receipt = http("connections/" + connection + "/bridge", {
                    "id": uuid.uuid4().hex, "version":1, "operation":{"op":op, **kw}
                })
                assert receipt["body"]["kind"] != "error", receipt["body"]
                return receipt
            def active(connection):
                response = bridge(connection, "capabilities")
                names = {item["id"] for item in response["body"]["value"]["commands"]}
                return response["generation"] if {"session.new","session.cd"} <= names else None
            def action(connection, generation, command, **arguments):
                return bridge(connection, "command", generation=generation, command=command,
                              arguments=arguments)["body"]["value"]["value"]
            def state(connection, generation):
                return bridge(connection, "state", generation=generation,resource="runtime",
                              query={})["body"]["value"]["value"]
            def pwd(peer, directory):
                job = peer.call("ssmcp.exec.start", {"command":["/bin/pwd"],"mode":"pipes"})
                peer.call("ssmcp.exec.wait", {"job_id":job["job_id"],"wait_ms":5000})
                read = peer.call("ssmcp.exec.read", {"job_id":job["job_id"]})
                assert read["exit_code"] == 0 and read["stdout"].strip() == str(directory), read
            targets = http("sessions")["sessions"]
            first = next(row["target"] for row in targets if row["name"] == "primary")
            primary = attach(first)
            pgen = wait_for(lambda: active(primary),"primary session bridge")
            assert state(primary, pgen)["process_id"] == parent.pid
            assert action(primary,pgen,"session.cd",path="project")["cwd"] == str(sub)
            pwd(test.connection("primary"),sub)
            created = action(primary,pgen,"session.new")
            assert created["session"] != first and created["cwd"] == str(sub)
            secondary = attach(created["session"])
            sgen = wait_for(lambda: active(secondary), "child session bridge", timeout=15)
            second_state = state(secondary,sgen)
            assert second_state["process_id"] != parent.pid
            assert second_state["client_id"] != state(primary,pgen)["client_id"]
            assert second_state["cwd"] == str(sub)
            assert action(secondary,sgen,"session.cd",path="child only")["cwd"] == str(nested)
            pwd(test.state.sessions[second_state["client_id"]],nested)
            pwd(test.connection("primary"),sub)
            assert state(primary,pgen)["cwd"] == str(sub)
            print("PASS: distinct process, relay identity and private attach target; child inherits cwd; both sessions execute pwd in their own directories", flush=True)
            check_tui(test, workspace, sub, nested, http, attach, bridge, state)
        except Exception:
            for _, err, _, _ in test.logs:
                print(test.redacted(err.read_text(errors="replace")[-1500:]), flush=True)
            print("fixture errors:", test.state.errors)
            raise
        finally:
            test.cleanup()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", required=True, type=Path)
    run(parser.parse_args().binary.resolve())
