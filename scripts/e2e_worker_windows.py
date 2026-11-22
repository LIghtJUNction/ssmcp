#!/usr/bin/env python3
"""Exercise worker windows with real clients, a private relay and private PTYs.

Requires an already-built ssmcp client, vt_snapshot, and library router fixture.
No Cargo invocation, public service, desktop input, installed terminal or model
is used. The terminal fixture also models a launcher that exits immediately.

Example:
  python3 scripts/e2e_worker_windows.py --bin-dir /tmp/candidate/bin \
    --fixture-binary /tmp/candidate/ssmcp-tests \
    --decoder /tmp/candidate/vt_snapshot --report /tmp/worker-windows.json
"""

from __future__ import annotations

import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import selectors
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time


CHILD_FLAG = "--ssmcp-worker-window-child"
FIXTURE_DIR_ENV = "SSMCP_E2E_WORKER_WINDOW_DIR"
ROWS, COLS = 58, 132


def record(directory, kind, **fields):
    path = Path(directory) / "terminal-events.jsonl"
    descriptor = os.open(path, os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    try:
        os.write(descriptor, (json.dumps({"kind": kind, **fields}, ensure_ascii=False)
                              + "\n").encode())
    finally:
        os.close(descriptor)


def process_identity(pid):
    try:
        directory = Path(f"/proc/{int(pid)}")
        fields = (directory / "stat").read_text().rsplit(")", 1)[1].split()
        if fields[0] == "Z":
            return None
        return [int(pid), int(fields[19]), directory.stat().st_uid]
    except (FileNotFoundError, ProcessLookupError):
        return None


def signal_owned(identity, number):
    if (not identity or identity[0] <= 1 or identity[0] == os.getpid()
            or identity[2] != os.getuid()):
        raise AssertionError("invalid owned process identity")
    if process_identity(identity[0]) != identity:
        return
    try:
        descriptor = os.pidfd_open(identity[0])
    except ProcessLookupError:
        return
    try:
        if process_identity(identity[0]) == identity:
            try:
                signal.pidfd_send_signal(descriptor, number)
            except ProcessLookupError:
                pass
    finally:
        os.close(descriptor)


def own_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)
    os.tcsetpgrp(0, os.getpgrp())


def fixture_terminal(arguments):
    """Implement xterm's argv boundary; never interpret a command string."""
    directory = Path(os.environ[FIXTURE_DIR_ENV])
    record(directory, "launcher", identity=process_identity(os.getpid()),
           argv=arguments, cwd=os.getcwd())
    if "-e" not in arguments:
        record(directory, "fixture_error", error="expected an xterm -e argv boundary")
        return 70
    command = arguments[arguments.index("-e") + 1:]
    if not command or CHILD_FLAG not in command:
        record(directory, "fixture_error", error="missing executable or child marker")
        return 71
    process = subprocess.Popen(
        [sys.executable, str(Path(__file__).resolve()), "--fixture-pty", *command],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    record(directory, "detached", identity=process_identity(process.pid))
    # Deliberately return before the child connects. Its supervisor is a separate
    # session, so tracking or killing this launcher alone cannot pass cleanup.
    return 0


def fixture_pty(command):
    directory = Path(os.environ[FIXTURE_DIR_ENV])
    master, slave = os.openpty()
    original = termios.tcgetattr(slave)
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    os.set_blocking(master, False)
    process = None
    stopping = False

    def stop(_number, _frame):
        nonlocal stopping
        stopping = True

    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    transcript = (directory / f"worker-{os.getpid()}.pty").open("xb", buffering=0)
    try:
        process = subprocess.Popen(command, stdin=slave, stdout=slave, stderr=slave,
                                   preexec_fn=own_terminal)
        worker_identity = process_identity(process.pid)
        record(directory, "worker", identity=worker_identity,
               supervisor=process_identity(os.getpid()), argv=command, cwd=os.getcwd())
        interrupted = False
        with selectors.DefaultSelector() as selector:
            selector.register(master, selectors.EVENT_READ)
            while process.poll() is None:
                if stopping:
                    process.terminate()
                    break
                if (directory / "close-monitor").is_file() and not interrupted:
                    os.write(master, b"\x03")
                    interrupted = True
                    record(directory, "monitor_interrupt")
                if selector.select(0.025):
                    try:
                        chunk = os.read(master, 65536)
                    except OSError as error:
                        if error.errno in (errno.EIO, errno.EAGAIN):
                            continue
                        raise
                    if chunk:
                        transcript.write(chunk)
            try:
                code = process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                code = process.wait(timeout=5)
                record(directory, "fixture_cleanup", reason="supervisor termination timed out")
            record(directory, "worker_exited", identity=worker_identity, exit_code=code,
                   terminal_restored=termios.tcgetattr(slave) == original)
            return 0
    except BaseException as error:
        record(directory, "fixture_error", error=f"{type(error).__name__}: {error}")
        raise
    finally:
        if process is not None and process.poll() is None:
            process.kill()
            process.wait(timeout=5)
            record(directory, "fixture_cleanup", reason="exception")
        transcript.close()
        os.close(master)
        os.close(slave)


if len(sys.argv) > 1 and sys.argv[1] == "--fixture-terminal":
    raise SystemExit(fixture_terminal(sys.argv[2:]))
if len(sys.argv) > 1 and sys.argv[1] == "--fixture-pty":
    raise SystemExit(fixture_pty(sys.argv[2:]))
if len(sys.argv) > 1 and sys.argv[1] == "--fixture-clipboard":
    record(Path(os.environ[FIXTURE_DIR_ENV]), "clipboard", argv=sys.argv[2:], text=sys.stdin.read())
    raise SystemExit(0)

from e2e import Harness, wait_for
from e2e_client_ui import prompt_text
from e2e_composer import input_text
from e2e_dashboard import Decoder


def digest(path):
    result = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for chunk in iter(lambda: stream.read(65536), b""):
            result.update(chunk)
    return result.hexdigest()


def events(directory):
    path = directory / "terminal-events.jsonl"
    if not path.exists():
        return []
    lines = path.read_text().splitlines(keepends=True)
    return [json.loads(line) for line in lines if line.endswith("\n")]


def empty_composer_ready(shot):
    # Do not hard-code the number of canvas columns: retired history views
    # change that count without changing whether the parent can accept input.
    return (shot["alternate_screen"] and "Message the agent" in prompt_text(shot)
            and not shot["hide_cursor"])


class Frontend:
    def __init__(self, harness, label, command, environment, cwd):
        self.decoder = Decoder(harness.decoder_binary)
        self.decoder.request(op="resize", rows=ROWS, cols=COLS)
        self.master, self.slave = os.openpty()
        self.original = termios.tcgetattr(self.slave)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
        os.set_blocking(self.master, False)
        self.transcript = bytearray()
        self.stdout = bytearray()
        self.process = subprocess.Popen(
            command, stdin=self.slave, stdout=subprocess.PIPE, stderr=self.slave,
            cwd=cwd, env=environment, preexec_fn=own_terminal,
        )
        harness.processes.append(self.process)
        harness.frontends.append(self)
        self.label = label
        self.closed = False
        os.set_blocking(self.process.stdout.fileno(), False)

    def pump(self, timeout=0.04):
        with selectors.DefaultSelector() as selector:
            selector.register(self.master, selectors.EVENT_READ, "pty")
            selector.register(self.process.stdout, selectors.EVENT_READ, "stdout")
            for key, _ in selector.select(timeout):
                try:
                    data = os.read(key.fd, 65536)
                except OSError as error:
                    if error.errno in (errno.EIO, errno.EAGAIN):
                        continue
                    raise
                if key.data == "pty":
                    self.transcript.extend(data)
                    self.decoder.feed(data)
                else:
                    self.stdout.extend(data)

    def frame(self):
        self.pump()
        return self.decoder.request(op="snapshot", cells=True)

    def until(self, predicate, description, timeout=15):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            frame = self.frame()
            if predicate(frame):
                return frame
            if self.process.poll() is not None:
                raise AssertionError(f"{description}: parent exited {self.process.returncode}")
        raise AssertionError(f"{description}: no matching physical frame")

    def send(self, data):
        os.write(self.master, data)

    def request_exit(self):
        # The ordinary chat frontend requires two Ctrl-C presses on an empty
        # composer. The worker monitor has its own single-press exit path.
        self.send(b"\x01\x0b\x03\x03")

    def wait_exit(self, timeout=15):
        deadline = time.monotonic() + timeout
        while self.process.poll() is None and time.monotonic() < deadline:
            self.pump()
        if self.process.poll() is None:
            raise AssertionError("parent did not exit within the cleanup deadline")
        self.pump(0.05)
        return self.process.returncode

    def close(self):
        if self.closed:
            return
        if self.process.poll() is None:
            self.request_exit()
            try:
                self.wait_exit()
            except AssertionError:
                Harness.stop(self.process)
        self.pump(0.02)
        self.decoder.close()
        self.process.stdout.close()
        os.close(self.master)
        os.close(self.slave)
        self.closed = True


class WorkerWindows(Harness):
    def __init__(self, directory, binary_dir, decoder, fixture_binary):
        super().__init__(directory, binary_dir)
        self.decoder_binary = decoder
        self.fixture_binary = fixture_binary
        self.fixture_timeout_seconds = 300
        self.frontends = []
        self.windows = []
        self.owned = []
        self.cases = {}
        self.cleanup_errors = []
        self.persistence_checks = []
        self.coordinators = {}
        self.worker_agents = {}
        self.clipboard_expectations = {}

    def environment(self, label, directory, *, terminal=True):
        self.windows.append(directory)
        environment = self.client_environment(label)
        allowed = {"SSMCP_AUTH_HOME", "SSMCP_SESSION_HOME", "SSMCP_CLIENT_CONFIG", "SSMCP_UI_CONFIG"}
        environment = {key: value for key, value in environment.items()
                       if not key.startswith("SSMCP_") or key in allowed}
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "ZTTY_SOCKET", "TERMINAL"):
            environment.pop(key, None)
        environment.update(TERM="xterm-256color", NO_COLOR="1", COLUMNS=str(COLS), LINES=str(ROWS),
                           GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                           GIT_CONFIG_COUNT="0", PYTHONDONTWRITEBYTECODE="1")
        # All implicit user-state locations point into this fixture. In
        # particular, old conversation and context-memory implementations used
        # XDG_STATE_HOME, so a regression cannot read or alter real user data.
        for variable, folder in (("XDG_STATE_HOME", "state"), ("XDG_CONFIG_HOME", "config"),
                                 ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache"),
                                 ("XDG_RUNTIME_DIR", "runtime"), ("TMPDIR", "tmp")):
            destination = directory / folder
            destination.mkdir(mode=0o700)
            environment[variable] = str(destination)
        environment[FIXTURE_DIR_ENV] = str(directory)
        clipboard = Path(environment["PATH"]) / "wl-copy"
        clipboard.write_text(
            f"#!{sys.executable}\nimport runpy, sys\n"
            f"sys.argv = [{str(Path(__file__).resolve())!r}, '--fixture-clipboard', *sys.argv[1:]]\n"
            f"runpy.run_path({str(Path(__file__).resolve())!r}, run_name='__main__')\n"
        )
        clipboard.chmod(0o700)
        # No host display socket is used. The only available clipboard program
        # is the private stdin recorder installed above.
        environment["WAYLAND_DISPLAY"] = "ssmcp-private-fixture"
        if terminal:
            folder = directory / "terminal path 'quoted' $literal"
            folder.mkdir(mode=0o700)
            launcher = folder / "xterm"
            launcher.write_text(
                f"#!{sys.executable}\nimport runpy, sys\n"
                f"sys.argv = [{str(Path(__file__).resolve())!r}, '--fixture-terminal', *sys.argv[1:]]\n"
                f"runpy.run_path({str(Path(__file__).resolve())!r}, run_name='__main__')\n"
            )
            launcher.chmod(0o700)
            environment["SSMCP_WORKER_TERMINAL"] = str(launcher)
        else:
            environment["SSMCP_WORKER_TERMINAL"] = str(directory / "absent-xterm")
        return environment

    def assert_no_persisted_conversation_state(self, stage):
        # Inspect only the test-owned tree. Terminal/service receipts and the
        # explicitly requested report are fixture artifacts, not client state.
        namespaces = {"conversations", "context-memory"}
        forbidden = sorted(str(path.relative_to(self.directory))
                           for path in self.directory.rglob("*")
                           if path.is_file()
                           and namespaces.intersection(path.relative_to(self.directory).parts))
        self.persistence_checks.append({"stage": stage, "unexpected_files": forbidden})
        self.check(not forbidden, f"{stage}: no conversation or context-memory files were created: {forbidden}")

    def assert_clipboard_counts(self):
        for directory, expected in self.clipboard_expectations.items():
            actual = len([entry for entry in events(directory) if entry["kind"] == "clipboard"])
            self.check(actual == expected,
                       "after parent exit: only the two requested non-full invitations reached the clipboard")

    def sessions(self):
        status, result, _ = self.request("GET", path="/api/client/sessions", secret=self.account_token)
        if status != 200 or not isinstance(result, dict):
            raise AssertionError("cannot read the private account's client list")
        return result["clients"]

    def select_pair(self, frontend, workspace, *, excluding=()):
        def connected():
            frontend.pump()
            current = [client for client in self.sessions()
                       if client.get("cwd") == str(workspace) and client["id"] not in excluding]
            parents = [client for client in current if client.get("chat_enabled") is True]
            children = [client for client in current if client.get("chat_enabled") is False]
            if len(parents) == 1 and len(children) == 1 and len(current) == 2:
                return parents[0], children[0]
            if frontend.process.poll() is not None:
                raise AssertionError("parent exited before both client connections were ready")
            return None
        return wait_for(connected, timeout=20, description="one chat parent and one worker connection")

    def join(self, client_id, name, role="main"):
        joined = self.public_call("collab", {"action": "join", "client_id": client_id,
                                           "name": name, "role": role, "capabilities": []})
        self.sensitive.append(joined["token"])
        return joined

    def bound(self, client_id, participant, name, arguments=None, *, allow_error=False):
        code = "const response = await call(%s, %s, %s); return response;" % (
            json.dumps(client_id), json.dumps(name), json.dumps(arguments or {}),
        )
        result = self.public_call("code_mode", {
            "agent": {"client_id": client_id, "id": participant["agent_id"], "token": participant["token"]},
            "code": code,
        })
        native = result["result"]
        if allow_error:
            return native
        if native.get("isError"):
            raise AssertionError(f"{name} returned a native error: {native}")
        return native["structuredContent"]

    def execute_fixture_command(self, client_id, participant):
        command = [sys.executable, "-c", "print('WINDOW_COMMAND_OK')"]
        code = "return await exec.run(%s, %s);" % (
            json.dumps(client_id), json.dumps({"command": command, "wait_ms": 5000, "max_bytes": 4096}),
        )
        response = self.public_call("code_mode", {
            "agent": {"client_id": client_id, "id": participant["agent_id"], "token": participant["token"]},
            "code": code,
        })["result"]
        self.check(response.get("exit_code") == 0 and response.get("stdout", "").strip() == "WINDOW_COMMAND_OK",
                   "the assigned external worker executes a real private command successfully")
        return response

    def pool_projection(self, frontend, parent, predicate, description):
        def projected():
            frontend.pump()
            response = self.bound(parent["id"], self.coordinators[parent["id"]],
                                  "ssmcp.ui.get", {"path": "/worker_pool"})
            if response.get("path") != "/worker_pool":
                raise AssertionError("worker-pool projection did not return its requested pointer")
            value = response.get("value")
            return value if isinstance(value, dict) and predicate(value) else None
        return wait_for(projected, timeout=20, description=description)

    def copied_invite(self, frontend, directory, command):
        before = len([entry for entry in events(directory) if entry["kind"] == "clipboard"])
        frontend.send(b"\x01\x0b" + command.encode() + b"\r")

        def copied():
            frontend.pump()
            copies = [entry for entry in events(directory) if entry["kind"] == "clipboard"]
            return copies[-1] if len(copies) > before else None

        copy = wait_for(copied, timeout=10, description=f"{command} private clipboard delivery")
        self.check(copy["argv"] == ["--type", "text/plain;charset=utf-8"],
                   f"{command}: prompt reaches the private clipboard through stdin")
        frontend.send(b"\x1b")
        frontend.until(empty_composer_ready, f"main composer after {command}")
        return copy["text"]

    def register_waiting_workers(self, frontend, parent, worker):
        executors = []
        for index in range(3):
            executor = self.join(worker["id"], f"window-executor-{index + 1}", role="worker")
            executor["wait_request_id"] = f"worker-window-ready-{index + 1}"
            waiting = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
                "request_id": executor["wait_request_id"], "wait_ms": 0,
            })
            self.check(waiting["status"] == "waiting", f"external worker {index + 1} actively waits")
            executors.append(executor)
        self.pool_projection(frontend, parent,
                             lambda pool: pool["registered_workers"] == 3 and pool["idle"] == 3
                             and pool["offline"] == 0 and pool["working"] == 0,
                             "three registered waiting workers in the parent projection")
        self.check(True, "registered workers and idle stations agree after all three workers wait")
        self.worker_agents[worker["id"]] = executors
        return executors

    def invitations_and_resize(self, frontend, directory, parent, worker, manager):
        main_prompt = self.copied_invite(frontend, directory, "/invite")
        self.check(parent["id"] in main_prompt,
                   "main invitation identifies the actual parent connection")
        worker_prompt = self.copied_invite(frontend, directory, "/invite worker")
        self.check(worker["id"] in worker_prompt and "worker.wait" in worker_prompt,
                   "worker invitation identifies the linked pool and active wait workflow")
        self.clipboard_expectations[directory] = 2
        executors = self.register_waiting_workers(frontend, parent, worker)
        before = len([entry for entry in events(directory) if entry["kind"] == "clipboard"])
        frontend.send(b"\x01\x0b/invite worker\r")
        frontend.until(lambda shot: "Worker stations are fully staffed." in "\n".join(shot["rows"]),
                       "full worker invitation refusal")
        self.check(len([entry for entry in events(directory) if entry["kind"] == "clipboard"]) == before,
                   "a full pool explains the limit without generating another invitation")
        frontend.send(b"\x1b")
        frontend.until(empty_composer_ready, "main composer after full-pool notice")
        before_resize = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]["slots"]
        waiting_slots = {(slot["slot"], slot["epoch"]) for slot in before_resize if slot["state"] == "idle"}
        self.check(len(waiting_slots) == 3, "three exact waiting station identities precede resizing")
        previous = self.pool_projection(frontend, parent, lambda pool: pool["total"] == 3,
                                        "worker resize receipt baseline").get("last_resize")
        previous_request = previous.get("request_id") if isinstance(previous, dict) else None
        frontend.send(b"\x01\x0b/worker +1\r")
        grown = self.pool_projection(frontend, parent,
                                    lambda pool: pool["total"] == 4 and pool["offline"] == 1
                                    and isinstance(pool.get("last_resize"), dict)
                                    and pool["last_resize"].get("accepted") is True
                                    and pool["last_resize"].get("target") == 4
                                    and pool["last_resize"].get("request_id") != previous_request,
                                    "one new offline worker station")
        self.check(grown["registered_workers"] == 3 and grown["idle"] == 3,
                   "growing the pool creates capacity without inventing a connected worker")
        self.check(grown["desired_total"] == 4 and grown["last_resize"].get("error") is None,
                   "growth has an accepted receipt from the actual worker pool")
        grown_slots = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]["slots"]
        offline_slots = {(slot["slot"], slot["epoch"]) for slot in grown_slots if slot["state"] == "offline"}
        self.check(len(offline_slots) == 1 and waiting_slots.isdisjoint(offline_slots),
                   "the added offline station has a distinct identity")
        frontend.send(b"\x1b\x01\x0b/worker -1\r")
        shrunk = self.pool_projection(frontend, parent,
                                      lambda pool: pool["total"] == 3 and pool["idle"] == 3
                                      and pool["offline"] == 0
                                      and isinstance(pool.get("last_resize"), dict)
                                      and pool["last_resize"].get("accepted") is True
                                      and pool["last_resize"].get("target") == 3
                                      and pool["last_resize"].get("request_id") != grown["last_resize"]["request_id"],
                                      "offline station removed before waiting workers")
        self.check(shrunk["desired_total"] == 3 and shrunk["last_resize"].get("error") is None,
                   "shrink acknowledgement belongs to the new resize request")
        state = self.bound(worker["id"], manager, "ssmcp.worker.status")
        self.check(state["worker"]["total"] == 3 and state["worker"]["idle"] == 3,
                   "resizing through the main window updates its actual linked pool")
        remaining = {(slot["slot"], slot["epoch"]) for slot in state["worker"]["slots"]}
        self.check(remaining == waiting_slots and remaining.isdisjoint(offline_slots),
                   "shrinking preserves all original waiting station identities and removes the offline addition")
        frontend.send(b"\x1b")
        frontend.until(empty_composer_ready, "main composer after worker resizing")
        return executors[0]

    def retire_cancelled_pool(self, frontend, parent, worker, manager, executor, claim, task_id):
        previous = self.pool_projection(frontend, parent, lambda pool: pool["total"] == 3,
                                        "three occupied or waiting stations before retirement")
        previous_request = previous["last_resize"]["request_id"]
        frontend.send(b"\x01\x0b/worker -3\r")
        shrinking = self.pool_projection(
            frontend, parent,
            lambda pool: pool["total"] == 3 and pool["desired_total"] == 0
            and pool["working"] == 1 and pool["retiring"] == 2
            and isinstance(pool.get("last_resize"), dict)
            and pool["last_resize"].get("accepted") is True
            and pool["last_resize"].get("target") == 0
            and pool["last_resize"].get("request_id") != previous_request,
            "zero-capacity request waits for the active task and two idle-worker acknowledgements",
        )
        self.check(shrinking["registered_workers"] == 3 and shrinking["idle"] == 0
                   and shrinking["last_resize"].get("error") is None,
                   "an accepted shrink does not claim that registered external workers have stopped")
        for idle_worker in self.worker_agents[worker["id"]]:
            if idle_worker["agent_id"] == executor["agent_id"]:
                continue
            retirement = self.bound(worker["id"], idle_worker, "ssmcp.worker.wait", {
                "request_id": idle_worker["wait_request_id"], "wait_ms": 0,
            })
            self.check(retirement["status"] == "retiring", "an idle worker receives its actual retirement receipt")
            receipt = retirement["retirement"]
            denied = self.bound(worker["id"], manager, "ssmcp.worker.release", {
                "retirement": receipt,
            }, allow_error=True)
            self.check(denied.get("isError") is True, "the manager cannot acknowledge another worker's retirement")
            foreign = self.bound(worker["id"], executor, "ssmcp.worker.release", {
                "retirement": receipt,
            }, allow_error=True)
            self.check(foreign.get("isError") is True,
                       "another registered worker cannot acknowledge the idle worker's retirement")
            released = self.bound(worker["id"], idle_worker, "ssmcp.worker.release", {"retirement": receipt})
            self.check(released["status"] == "released" and released["duplicate"] is False,
                       "the original idle worker releases its exact station")
            repeated = self.bound(worker["id"], idle_worker, "ssmcp.worker.release", {"retirement": receipt})
            self.check(repeated["status"] == "released" and repeated["duplicate"] is True,
                       "retrying the same retirement acknowledgement is idempotent")

        waiting = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-cancel-claim", "wait_ms": 0,
        })
        self.check(waiting["status"] == "assigned" and waiting["cancel_requested"] is True
                   and waiting["retire_requested"] is True and waiting["claim"] == claim
                   and waiting["worker"]["working"] == 1 and waiting["worker"]["total"] == 1,
                   "a busy retiring worker retains its exact claim until it acknowledges cancellation")
        self.bound(worker["id"], executor, "ssmcp.worker.complete", {"claim": claim, "status": "cancelled"})
        result = self.bound(worker["id"], manager, "ssmcp.worker.result", {"task_id": task_id})
        self.check(result["task"]["status"] == "cancelled" and result["worker"]["working"] == 0
                   and result["worker"]["retiring"] == 1 and result["worker"]["idle"] == 0,
                   "completion preserves the result while the final station awaits its worker's release")
        completed_retry = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-cancel-claim", "wait_ms": 0,
        })
        self.check(completed_retry["status"] == "already_completed" and completed_retry["claim"] == claim,
                   "the completed request retains its receipt during retirement")
        retirement = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-retire-after-cancel", "wait_ms": 0,
        })
        self.check(retirement["status"] == "retiring", "a new wait request obtains the final retirement receipt")
        released = self.bound(worker["id"], executor, "ssmcp.worker.release", {
            "retirement": retirement["retirement"],
        })
        self.check(released["status"] == "released" and released["worker"]["total"] == 0
                   and released["worker"]["registered_workers"] == 0,
                   "capacity reaches zero only after every original worker acknowledges retirement")
        after_release = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-released-observation", "wait_ms": 0,
        })
        self.check(after_release["status"] == "released",
                   "a released worker is instructed to stop waiting rather than registering again")
        self.pool_projection(frontend, parent,
                             lambda pool: pool["total"] == 0 and pool["desired_total"] == 0
                             and pool["retiring"] == 0 and pool["registered_workers"] == 0
                             and pool.get("connected") is True,
                             "the main window observes a connected empty worker pool")
        frontend.send(b"\x1b\x01\x0b/worker +3\r")
        regrown = self.pool_projection(
            frontend, parent,
            lambda pool: pool["total"] == 3 and pool["desired_total"] == 3
            and pool["offline"] == 3 and pool["registered_workers"] == 0
            and isinstance(pool.get("last_resize"), dict)
            and pool["last_resize"].get("accepted") is True
            and pool["last_resize"].get("target") == 3
            and pool["last_resize"].get("request_id") != shrinking["last_resize"]["request_id"],
            "growth after complete retirement creates three genuinely unstaffed stations",
        )
        self.check(regrown["idle"] == 0 and regrown["working"] == 0 and regrown["retiring"] == 0,
                   "old collaboration memberships are not counted as newly waiting workers")
        retained = self.bound(worker["id"], manager, "ssmcp.worker.result", {"task_id": task_id})
        self.check(retained["task"]["status"] == "cancelled",
                   "retiring and replacing a station preserves the original task's confirmed result")
        frontend.send(b"\x1b")
        frontend.until(empty_composer_ready, "main composer after retirement and regrowth")

    def launch(self, label):
        directory = self.directory / label
        directory.mkdir(mode=0o700)
        workspace = directory / "workspace 'single' \"double\" $literal 中文"
        workspace.mkdir(mode=0o700)
        environment = self.environment(label, directory)
        name = f"{label} 'single' \"double\" $literal 中文"
        command = self.client_command(name=name, extra_args=("--worker", "3", "--no-watchdog"))
        frontend = Frontend(self, label, command, environment, workspace)
        parent, worker = self.select_pair(frontend, workspace)
        if not self.secret:
            self.oauth_login({"client_id": parent["id"]})
        frontend.until(empty_composer_ready, "ordinary parent conversation and composer")
        self.check(parent["id"] != worker["id"], f"{label}: parent and worker have separate client identities")
        self.check(parent.get("chat_enabled") and worker.get("chat_enabled") is False,
                   f"{label}: only the parent exposes human chat")
        self.coordinators[parent["id"]] = self.join(parent["id"], "parent-coordinator")
        manager = self.join(worker["id"], "window-manager")
        state = self.bound(worker["id"], manager, "ssmcp.worker.status")
        self.check(state["worker"]["total"] == 3 and state["worker"]["offline"] == 3
                   and state["worker"]["idle"] == 0,
                   f"{label}: all three stations stay offline until external workers wait")
        records = wait_for(lambda: [entry for entry in events(directory) if entry["kind"] == "worker"],
                           timeout=5, description="actual worker process receipt")
        self.check(len(records) == 1, f"{label}: the child does not recursively open another terminal")
        child = records[0]
        self.owned.extend([child["identity"], child["supervisor"]])
        self.check(child["identity"] is not None and state["worker"]["process_id"] == child["identity"][0],
                   f"{label}: status identifies the actual worker process")
        projection = self.pool_projection(frontend, parent,
                                          lambda pool: pool.get("client_id") == worker["id"]
                                          and pool.get("connected") is True,
                                          "parent links the actual connected worker process")
        self.check(projection["process_id"] == child["identity"][0]
                   and projection["total"] == 3 and projection["registered_workers"] == 0
                   and projection["offline"] == 3 and projection["idle"] == 0,
                   f"{label}: /worker_pool carries the real process and unstaffed capacity")
        self.check(child["cwd"] == str(workspace) and worker["cwd"] == str(workspace),
                   f"{label}: the child preserves the working directory literally")
        self.check(child["argv"].count(CHILD_FLAG) == 1 and name in child["argv"],
                   f"{label}: quoted, spaced, dollar and Unicode arguments cross the terminal unchanged")
        launchers = [entry for entry in events(directory) if entry["kind"] == "launcher"]
        self.check(len(launchers) == 1, f"{label}: only one terminal launcher was called")
        launcher = launchers[0]["identity"]
        wait_for(lambda: process_identity(launcher[0]) != launcher, timeout=5,
                 description="detached terminal launcher exit")
        self.check(process_identity(child["identity"][0]) == child["identity"],
                   f"{label}: the actual worker remains live after its launcher exits")
        self.cases[label] = {"parent_pid": frontend.process.pid, "worker_pid": child["identity"][0],
                             "launcher_pid": launcher[0], "slots": 3}
        return frontend, directory, workspace, parent, worker, manager

    def protocol(self, frontend, directory, parent, worker, manager):
        attached = self.bound(worker["id"], manager, "ssmcp.worker.attach")
        self.check(attached["caller_is_manager"], "the main participant attaches to the discovered worker pool")
        executor = self.invitations_and_resize(frontend, directory, parent, worker, manager)
        members = self.public_call("collab", {"action": "list", "client_id": worker["id"]})
        self.check(any(member["agent_id"] == executor["agent_id"] and member["role"] == "worker"
                       for member in members["agents"]),
                   "the external executor explicitly registers as a worker before waiting")
        denied = self.bound(worker["id"], executor, "ssmcp.worker.attach", allow_error=True)
        self.check(denied.get("isError") is True, "an executor cannot take over the pool manager")
        submitted = self.bound(worker["id"], manager, "ssmcp.worker.submit", {
            "request_id": "worker-windows-task-one", "text": "Return WINDOW_TASK_OK from this private fixture.",
        })
        task = submitted.get("task", submitted)
        task_id = task["task_id"]
        assignment = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": executor["wait_request_id"], "wait_ms": 1000,
        })
        self.check(assignment["status"] == "assigned" and assignment["claim"]["task_id"] == task_id,
                   "one authenticated executor receives the submitted task")
        duplicate = self.bound(worker["id"], executor, "ssmcp.worker.claim", {
            "request_id": executor["wait_request_id"],
        })
        self.check(duplicate["claim"] == assignment["claim"], "claim retry retains the exact assigned lease")
        self.execute_fixture_command(worker["id"], executor)
        command_state = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
        command_slot = next(slot for slot in command_state["slots"]
                            if slot["slot"] == assignment["claim"]["slot"])
        command_logs = [entry for entry in command_slot["logs"] if "python" in entry["text"].lower()]
        self.check("python" in (command_slot.get("last_command") or "").lower()
                   and bool(command_logs) and all(entry.get("task_id") == task_id for entry in command_logs),
                   "before any progress report, the real command already has a correctly attributed log")
        progress = "The private Python command returned WINDOW_COMMAND_OK with exit code 0."
        report_arguments = {"claim": assignment["claim"], "request_id": "worker-window-progress-one",
                            "event": "progress", "progress": progress}
        reported = self.bound(worker["id"], executor, "ssmcp.worker.report", report_arguments)
        self.check(reported["status"] == "recorded" and reported["duplicate"] is False,
                   "the claimant reports an observation after the command really completed")
        repeated = self.bound(worker["id"], executor, "ssmcp.worker.report", report_arguments)
        self.check(repeated["duplicate"] is True, "an identical progress retry does not create a second event")
        rejected = self.bound(worker["id"], manager, "ssmcp.worker.report", {
            **report_arguments, "request_id": "worker-window-foreign-progress", "progress": "UNAUTHORIZED_PROGRESS",
        }, allow_error=True)
        self.check(rejected.get("isError") is True, "the manager cannot report activity as another claim's executor")
        active = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
        active_slot = next(slot for slot in active["slots"] if slot["slot"] == assignment["claim"]["slot"])
        self.check("WINDOW_TASK_OK" in (active_slot.get("task_summary") or "")
                   and "python" in (active_slot.get("last_command") or "").lower()
                   and all(entry in active_slot["logs"] for entry in command_logs),
                   "monitor summary and command log are attributed to the actual claimed task")
        self.check(active_slot.get("progress") == progress
                   and sum(progress in entry["text"] for entry in active_slot["logs"]) == 1,
                   "the monitor retains one truthful, authenticated progress observation")
        self.check(all(not slot.get("last_command") for slot in active["slots"]
                       if slot["slot"] != active_slot["slot"]),
                   "other waiting workers do not inherit the active worker's command")
        elapsed = active_slot.get("current_task_elapsed_ms")
        self.check(isinstance(elapsed, int) and elapsed >= 0 and active_slot["total_work_ms"] >= elapsed,
                   "current work time is present while the real task is active")
        self.bound(worker["id"], executor, "ssmcp.worker.complete", {
            "claim": assignment["claim"], "status": "completed", "output": "WINDOW_TASK_OK",
        })
        result = self.bound(worker["id"], manager, "ssmcp.worker.result", {"task_id": task_id})
        task = result.get("task", result)
        self.check(task["status"] == "completed" and task["output"] == "WINDOW_TASK_OK",
                   "the main participant retrieves the exact worker result through the real relay")
        status = self.bound(worker["id"], manager, "ssmcp.worker.status")
        self.check(status["worker"]["idle"] == 3 and status["worker"]["working"] == 0,
                   "completion returns the child pool to three idle slots")
        completed_slot = next(slot for slot in status["worker"]["slots"]
                              if slot["slot"] == assignment["claim"]["slot"])
        durations = completed_slot["recent_task_durations_ms"]
        self.check(completed_slot["current_task_elapsed_ms"] is None
                   and len(durations) == 1 and durations[0] >= elapsed
                   and completed_slot["total_work_ms"] == durations[0],
                   "completion freezes one real task duration without a fabricated active timer")
        cancelled = self.bound(worker["id"], manager, "ssmcp.worker.submit", {
            "request_id": "worker-windows-cancel-task", "text": "Wait for cancellation in this private fixture.",
        })["task"]["task_id"]
        pending = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-cancel-claim", "wait_ms": 1000,
        })
        next_task = self.bound(worker["id"], manager, "ssmcp.worker.status")["worker"]
        next_slot = next(slot for slot in next_task["slots"] if slot["slot"] == pending["claim"]["slot"])
        self.check(next_slot.get("last_command") is None and next_slot.get("progress") is None,
                   "a new task clears the previous task's command and progress fields")
        cancellation = self.bound(worker["id"], manager, "ssmcp.worker.cancel", {"task_id": cancelled})
        self.check(cancellation["task"]["status"] == "cancelling"
                   and cancellation["worker"]["working"] == 1,
                   "cancellation keeps a remote task occupied until its executor acknowledges")
        waiting = self.bound(worker["id"], executor, "ssmcp.worker.wait", {
            "request_id": "worker-windows-cancel-claim", "wait_ms": 0,
        })
        self.check(waiting["cancel_requested"] is True and waiting["claim"] == pending["claim"],
                   "the original claimant observes cancellation through the same claim")
        self.retire_cancelled_pool(frontend, parent, worker, manager, executor, pending["claim"], cancelled)
        parent_agent = self.coordinators[parent["id"]]
        self.bound(parent["id"], parent_agent, "ssmcp.ui.message", {
            "text": "WINDOW_TASK_OK", "kind": "progress",
        })
        frontend.until(lambda shot: "WINDOW_TASK_OK" in "\n".join(shot["rows"]),
                       "worker result displayed in the parent conversation")
        self.check(True, "the parent conversation receives the independently completed worker result")
        self.check_composer(frontend, "parent remains editable during coordination")

    def check_composer(self, frontend, label):
        marker = "WINDOW_CHAT_中文_$literal"
        frontend.send(b"\x1b[200~" + marker.encode() + b"\x1b[201~")
        frontend.until(lambda shot: marker in input_text(shot), label)
        self.check(True, label)
        frontend.send(b"\x01\x0b")

    def assert_children_gone(self, directory):
        identities = []
        workers = []
        for entry in events(directory):
            if entry["kind"] == "worker":
                identities.extend([entry["identity"], entry["supervisor"]])
                workers.append(entry["identity"])
        wait_for(lambda: all(process_identity(item[0]) != item for item in identities if item),
                 timeout=15, description="owned worker and detached terminal supervisor exit")
        self.check(bool(identities), "owned worker cleanup was observed for actual processes")
        self.check(not any(entry["kind"] in ("fixture_cleanup", "fixture_error") for entry in events(directory)),
                   "worker cleanup completed before fixture fallback or error handling")
        ended = [entry for entry in events(directory) if entry["kind"] == "worker_exited"]
        exited = [entry.get("identity") for entry in ended]
        self.check(bool(workers) and len(ended) == len(workers)
                   and all(identity is not None and exited.count(identity) == 1 for identity in workers)
                   and all(entry["exit_code"] == 0 and entry["terminal_restored"] for entry in ended),
                   "every actual worker has its own normal-exit and restored-terminal receipt")

    def default_and_parent_exit(self):
        frontend, directory, _, parent, worker, manager = self.launch("parent-exit")
        self.protocol(frontend, directory, parent, worker, manager)
        self.assert_no_persisted_conversation_state("active coordination")
        frontend.request_exit()
        self.check(frontend.wait_exit() == 0, "parent exits normally after two Ctrl-C presses")
        self.assert_children_gone(directory)
        self.assert_clipboard_counts()
        self.check(termios.tcgetattr(frontend.slave) == frontend.original,
                   "parent exit restores its private terminal settings")

    def worker_close(self):
        frontend, directory, _, parent, worker, _ = self.launch("worker-close")
        (directory / "close-monitor").touch()
        self.assert_children_gone(directory)
        wait_for(lambda: all(client["id"] != worker["id"] for client in self.sessions()),
                 timeout=10, description="closed monitor removed from relay inventory")
        self.check(frontend.process.poll() is None and any(client["id"] == parent["id"] for client in self.sessions()),
                   "closing the worker window preserves the main chat connection")
        self.check_composer(frontend, "main composer still accepts input after worker window closes")
        frontend.request_exit()
        self.check(frontend.wait_exit() == 0, "main chat exits independently after monitor closure")

    def parent_reload(self):
        frontend, directory, workspace, parent, worker, _ = self.launch("parent-reload")
        original = next(entry for entry in events(directory) if entry["kind"] == "worker")
        frontend.send(b"\x01\x0b/reload\r")
        new_parent, new_worker = self.select_pair(frontend, workspace,
                                                  excluding=(parent["id"], worker["id"]))
        wait_for(lambda: process_identity(original["identity"][0]) != original["identity"],
                 timeout=15, description="original worker removed during parent reload")
        records = wait_for(lambda: ([entry for entry in events(directory) if entry["kind"] == "worker"]
                                    if len([entry for entry in events(directory) if entry["kind"] == "worker"]) == 2
                                    else None), timeout=10, description="one replacement worker after reload")
        self.owned.extend([records[-1]["identity"], records[-1]["supervisor"]])
        self.check(new_parent["id"] != parent["id"] and new_worker["id"] != worker["id"],
                   "reload replaces both connection identities")
        self.check(len([entry for entry in events(directory) if entry["kind"] == "launcher"]) == 2,
                   "reload opens exactly one replacement worker window")
        manager = self.join(new_worker["id"], "replacement-manager")
        status = self.bound(new_worker["id"], manager, "ssmcp.worker.status")
        self.check(status["worker"]["total"] == 3 and status["worker"]["offline"] == 3,
                   "the replacement pool starts with three unstaffed stations")
        self.coordinators[new_parent["id"]] = self.join(new_parent["id"], "replacement-coordinator")
        self.pool_projection(frontend, new_parent,
                             lambda pool: pool["client_id"] == new_worker["id"]
                             and pool["process_id"] == records[-1]["identity"][0],
                             "reloaded parent projects the replacement worker process")
        frontend.until(empty_composer_ready, "interactive parent after reload")
        self.assert_no_persisted_conversation_state("parent reload")
        frontend.request_exit()
        self.check(frontend.wait_exit() == 0, "reloaded parent exits normally")
        self.assert_children_gone(directory)

    def headless(self):
        directory = self.directory / "headless"
        directory.mkdir(mode=0o700)
        environment = self.environment("headless", directory)
        command = self.client_command(name="headless-worker", extra_args=(
            "--worker", "3", "--input", "none", "--no-watchdog"))
        process = self.spawn("headless-worker", command, cwd=directory, env=environment)
        current = wait_for(lambda: next((client for client in self.sessions()
                                        if client.get("cwd") == str(directory)), None),
                           timeout=15, description="headless worker connection")
        participant = self.join(current["id"], "headless-manager")
        status = self.bound(current["id"], participant, "ssmcp.worker.status")
        self.check(current.get("chat_enabled") is False and status["worker"]["total"] == 3,
                   "explicit headless mode retains a direct three-slot worker pool")
        self.check(not events(directory), "headless mode does not invoke a terminal launcher")
        self.stop(process)

    def missing_terminal(self):
        directory = self.directory / "missing-terminal"
        directory.mkdir(mode=0o700)
        environment = self.environment("missing-terminal", directory, terminal=False)
        command = self.client_command(name="missing-terminal", extra_args=("--worker", "3", "--no-watchdog"))
        frontend = Frontend(self, "missing-terminal", command, environment, directory)
        self.check(frontend.wait_exit() != 0, "missing terminal produces a failing exit status")
        output = (frontend.stdout + frontend.transcript).decode(errors="replace").lower()
        self.check("terminal" in output and ("worker" in output or "window" in output),
                   "missing-terminal failure explains the worker window launch problem")
        wait_for(lambda: not any(client.get("cwd") == str(directory) for client in self.sessions()),
                 timeout=10, description="failed startup removed from the relay inventory")
        self.check(True, "failed window startup leaves no connected child")

    def daemon_dispatch(self):
        directory = self.directory / "daemon-dispatch"
        directory.mkdir(mode=0o700)
        environment = self.environment("daemon-dispatch", directory)
        # This narrowly checks dispatch into the existing background-service
        # path. Full user-systemd lifecycle coverage remains in e2e_daemon.py.
        service_log = directory / "service.jsonl"
        for name in ("systemctl", "systemd-run"):
            program = Path(environment["PATH"]) / name
            program.write_text(
                f"#!{sys.executable}\nimport json, sys\n"
                f"with open({str(service_log)!r}, 'a') as stream:\n"
                "    stream.write(json.dumps(sys.argv) + '\\n')\n"
                "sys.stderr.write('private fixture service manager unavailable\\n')\n"
                "raise SystemExit(77)\n"
            )
            program.chmod(0o700)
        command = self.client_command(name="daemon-worker", extra_args=("--worker", "3", "--daemon", "--no-watchdog"))
        result = subprocess.run(command, cwd=directory, env=environment, stdin=subprocess.DEVNULL,
                                capture_output=True, timeout=15)
        self.check(result.returncode != 0 and service_log.is_file(),
                   "daemon worker dispatches to the existing background service manager")
        self.check(not events(directory), "daemon dispatch never launches an interactive worker window")

    def run(self):
        self.prepare()
        self.default_and_parent_exit()
        self.worker_close()
        self.parent_reload()
        self.headless()
        self.missing_terminal()
        self.daemon_dispatch()
        self.assert_no_persisted_conversation_state("completed fixture cases")

    def cleanup(self):
        for frontend in reversed(self.frontends):
            try:
                frontend.close()
            except Exception as error:
                self.cleanup_errors.append(f"frontend cleanup: {error}")
        # Failure-path cleanup targets only PIDs captured from our private
        # launcher receipts, and checks the kernel start identity before signals.
        for directory in self.windows:
            for entry in events(directory):
                if entry["kind"] == "worker":
                    self.owned.extend([entry["identity"], entry["supervisor"]])
                elif entry["kind"] in ("detached", "launcher"):
                    # A launch may fail before the child can emit its worker
                    # receipt; its already-recorded supervisor still belongs to
                    # this fixture and must not escape failure-path cleanup.
                    self.owned.append(entry["identity"])
        for identity in self.owned:
            if identity and process_identity(identity[0]) == identity:
                try:
                    signal_owned(identity, signal.SIGTERM)
                except Exception as error:
                    self.cleanup_errors.append(f"owned process cleanup: {error}")
        deadline = time.monotonic() + 5
        while any(item and process_identity(item[0]) == item for item in self.owned) and time.monotonic() < deadline:
            time.sleep(0.02)
        for identity in self.owned:
            if identity and process_identity(identity[0]) == identity:
                signal_owned(identity, signal.SIGKILL)
        super().cleanup()
        if self.cleanup_errors:
            raise AssertionError("; ".join(self.cleanup_errors))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path,
                        default=os.environ.get("SSMCP_E2E_FIXTURE_BINARY"))
    parser.add_argument("--decoder", type=Path)
    parser.add_argument("--report", type=Path, required=True)
    parser.add_argument("--source-revision", default="unrecorded")
    args = parser.parse_args()
    binary_dir = args.bin_dir.resolve()
    decoder = (args.decoder or binary_dir / "examples" / "vt_snapshot").resolve()
    report = {"schema_version": 1, "suite": "worker_windows", "status": "blocked",
              "source_revision": args.source_revision, "checks": [], "cases": {},
              "scope": {"platform": "Linux private PTYs", "relay": "private production account/router fixture",
                        "terminal": "detached argv-preserving terminal fixture", "models_started": 0,
                        "desktop_input": False, "daemon": "dispatch only; full lifecycle: e2e_daemon.py"}}
    harness = None
    exit_code = 2
    try:
        if sys.platform != "linux" or not hasattr(os, "pidfd_open") or not hasattr(signal, "pidfd_send_signal"):
            raise RuntimeError("Linux pidfd support is required for isolated process cleanup")
        required = [binary_dir / "ssmcp", decoder]
        if args.fixture_binary is None:
            raise RuntimeError("supply an already-built --fixture-binary; this test never invokes Cargo")
        fixture_binary = Path(args.fixture_binary).resolve()
        required.append(fixture_binary)
        for path in required:
            if not path.is_file() or not os.access(path, os.X_OK):
                raise RuntimeError(f"required executable is unavailable: {path}")
        report["executables"] = [{"path": str(path), "sha256": digest(path)} for path in required]
        with tempfile.TemporaryDirectory(prefix="ssmcp-worker-windows-") as temporary:
            directory = Path(temporary)
            os.chmod(directory, 0o700)
            harness = WorkerWindows(directory, binary_dir, decoder, fixture_binary)
            report["status"] = "failed"
            try:
                harness.run()
                report["status"] = "passed"
                exit_code = 0
            finally:
                report["checks"] = harness.checks
                report["cases"] = harness.cases
                report["persistence"] = harness.persistence_checks
                if report["status"] != "passed":
                    report["diagnostics"] = {front.label: {
                        "stdout_tail": front.stdout[-4096:].decode(errors="replace"),
                        "terminal_tail": front.transcript[-4096:].decode(errors="replace"),
                        "exit_code": front.process.poll(),
                    } for front in harness.frontends}
                    report["terminal_events"] = {str(path.name): events(path) for path in harness.windows}
                harness.cleanup()
                harness.assert_no_persisted_conversation_state("after all processes exited")
                harness.assert_clipboard_counts()
                report["cleanup"] = {"errors": harness.cleanup_errors,
                                     "live_owned_processes": [item[0] for item in harness.owned
                                                               if item and process_identity(item[0]) == item]}
                if report["cleanup"]["live_owned_processes"]:
                    raise AssertionError("owned worker processes survived fixture cleanup")
    except Exception as error:
        exit_code = 2 if harness is None else 1
        report["status"] = "blocked" if harness is None else "failed"
        message = f"{type(error).__name__}: {error}"
        for secret in getattr(harness, "sensitive", []):
            message = message.replace(secret, "<fixture-secret>")
        report["error"] = message
    args.report.parent.mkdir(parents=True, exist_ok=True)
    encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
    for secret in getattr(harness, "sensitive", []):
        encoded = encoded.replace(secret, "<fixture-secret>")
    args.report.write_text(encoded)
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "report": str(args.report), "error": report.get("error")}, ensure_ascii=False))
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
