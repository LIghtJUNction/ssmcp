#!/usr/bin/env python3
"""Exercise conversation goals and cached startup notices on owned real terminals.

The release cache is explicitly synthetic. No public release is impersonated and
no external request is needed: a private proxy trap detects accidental discovery.
The caller must bind these prebuilt inputs and this source to its passed gate.
"""
import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import platform
import re
import selectors
import shutil
import signal
import socket
import stat
import struct
import subprocess
import tempfile
import termios
import time

from e2e_composer import DOWN, ENTER, HOME
from e2e_dashboard import Decoder, Frontend, text
from e2e_interactions import Interactions, digest

ROWS = 32
RELEASE = "v999.0.0"
GOAL_A = "GOAL_A 完成当前任务"
GOAL_B = "GOAL_B 保留另一会话目标"


def native_target():
    architecture = {"x86_64": "x86_64", "amd64": "x86_64",
                    "aarch64": "aarch64", "arm64": "aarch64"}.get(platform.machine().lower())
    suffix = {"Linux": "unknown-linux-gnu", "Darwin": "apple-darwin"}.get(platform.system())
    if not architecture or not suffix:
        raise ValueError("this owned POSIX terminal fixture needs a supported native host")
    return architecture + "-" + suffix


class AllTtyFrontend(Frontend):
    """Use a TTY for all three descriptors; discover identity through the account."""

    def __init__(self, harness, label, config, decoder):
        self.harness = harness
        self.decoder = Decoder(decoder)
        self.master, self.slave = os.openpty()
        self.original_termios = termios.tcgetattr(self.slave)
        self.transcript = bytearray()
        self.stdout = bytearray()
        self.closed = False
        self.input_bytes = 0
        self.decoder.request(op="resize", rows=ROWS, cols=120)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, 120, 0, 0))
        os.set_blocking(self.master, False)
        try:
            environment = harness.client_environment(label)
            git = shutil.which("git")
            if not git:
                raise RuntimeError("the fixture requires the already-installed Git")
            (Path(environment["PATH"]) / "git").symlink_to(Path(git).absolute())
            environment.update(harness.safe_git_env)
            environment.update({"SSMCP_UI_CONFIG": str(config), "TERM": "xterm-256color",
                                "NO_COLOR": "1", "COLUMNS": "120", "LINES": str(ROWS)})

            def own_terminal():
                os.setsid()
                fcntl.ioctl(0, termios.TIOCSCTTY, 0)
                os.tcsetpgrp(0, os.getpgrp())

            self.process = subprocess.Popen(
                harness.client_command(name=label), stdin=self.slave,
                stdout=self.slave, stderr=self.slave, cwd=harness.workspace,
                env=environment, preexec_fn=own_terminal,
            )
            harness.processes.append(self.process)
            self.credentials = harness.account_connection(self, label)
            self.client_id = self.credentials["client_id"]
        except BaseException:
            self.close()
            raise

    def pump(self, seconds=0.05):
        deadline = time.monotonic() + seconds
        output = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(self.master, selectors.EVENT_READ)
            while True:
                for key, _ in selector.select(max(0, deadline - time.monotonic())):
                    try:
                        block = os.read(key.fd, 65536)
                    except OSError as error:
                        if error.errno in (errno.EIO, errno.EAGAIN):
                            continue
                        raise
                    if not block:
                        selector.unregister(key.fileobj)
                    else:
                        output.extend(block)
                        self.transcript.extend(block)
                if time.monotonic() >= deadline or not selector.get_map():
                    break
        self.decoder.feed(output)

    def send(self, data):
        self.input_bytes += len(data.encode() if isinstance(data, str) else data)
        super().send(data)


class GoalStartup(Interactions):
    def __init__(self, directory, binary_dir, decoder, output):
        super().__init__(directory, binary_dir, decoder, output)
        self.fixture_timeout_seconds = 180
        self.target = native_target()
        self.proxy = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        self.proxy.bind(("127.0.0.1", 0))
        self.proxy.listen(16)
        self.proxy.setblocking(False)
        self.proxy_connections = 0
        self.labels = []
        self.frame_manifest = []
        self.cache = directory / "cache" / "ssmcp" / "update" / "release.json"
        self.history_root = self.state_home / "ssmcp" / "conversations"

    def client_environment(self, label, **options):
        environment = super().client_environment(label, **options)
        self.labels.append(label)
        for name in list(environment):
            if name.lower() in ("http_proxy", "https_proxy", "all_proxy", "no_proxy"):
                del environment[name]
        proxy = "http://127.0.0.1:" + str(self.proxy.getsockname()[1])
        for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"):
            environment[name] = proxy
        environment["NO_PROXY"] = environment["no_proxy"] = "127.0.0.1,localhost,::1"
        return environment

    def no_discovery(self, description):
        while True:
            try:
                connection, _ = self.proxy.accept()
            except BlockingIOError:
                break
            self.proxy_connections += 1
            connection.close()
        self.check(self.proxy_connections == 0, description)

    def seed_cache(self, *, expired=False):
        for directory in (self.cache.parent.parent, self.cache.parent):
            directory.mkdir(mode=0o700, exist_ok=True)
        value = {"schema": 2, "target": self.target,
                 "checked_at": int(time.time()) - (90000 if expired else 0),
                 "succeeded": True, "latest": RELEASE}
        with self.cache.open("w", encoding="utf-8") as stream:
            os.chmod(self.cache, 0o600)
            json.dump(value, stream, separators=(",", ":"))
        return self.cache.read_bytes()

    def account_connection(self, front, label):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            front.pump(0.05)
            if front.process.poll() is not None:
                raise AssertionError("owned all-TTY client exited before registration\n" + front.failure_detail())
            status, value, _ = self.request("GET", path="/api/client/sessions", secret=self.account_token)
            if status == 200 and isinstance(value, dict):
                matches = [item for item in value.get("clients", [])
                           if item.get("name") == label and item.get("cwd") == str(self.workspace)]
                if len(matches) > 1:
                    raise AssertionError("ambiguous owned all-TTY client identity")
                if matches:
                    return self.validate_connection_record({"client_id": matches[0]["id"]})
        raise AssertionError("the exact owned terminal did not register with its private account")

    def all_tty(self, label):
        front = AllTtyFrontend(self, label, self.config(label), self.decoder)
        self.frontends.append(front)
        self.last_frontend = front
        self.client_id = front.client_id
        return front

    def capture(self, front, name, marker, width=None):
        shot = front.until(lambda value: marker in text(value)
                           and (width is None or value["size"] == [ROWS, width]), name, cells=True)
        encoded = json.dumps(shot, ensure_ascii=False, indent=2) + "\n"
        self.check(not any(secret and secret in encoded for secret in [self.secret, *self.sensitive]),
                   name + ": the terminal snapshot has no fixture credential")
        path = self.output / (name + ".json")
        path.write_text(encoded, encoding="utf-8")
        (self.output / (name + ".txt")).write_text(text(shot) + "\n", encoding="utf-8")
        self.frames.append(path.name)
        self.frame_manifest.append({"path": path.name, "sha256": digest(path), "size": shot["size"]})
        return shot

    def capture_widths(self, front, name, marker):
        for width in (120, 48):
            front.resize(ROWS, width)
            front.pump(0.15)
            self.capture(front, name + "-" + str(width), marker, width)
        front.resize(ROWS, 120)
        front.pump(0.1)

    def local(self, front, command):
        self.replace_short(front, command)
        front.send(ENTER)
        return self.wait_state(front, lambda value: not value["draft"]["text"],
                               "local command consumed its owned input: " + command.split()[0])

    def goal(self, front, expected, conversation_id=None):
        state = self.wait_state(
            front,
            lambda value: isinstance(value.get("goal"), dict)
            and value["goal"].get("status") == ("unset" if expected is None else "set")
            and value["goal"].get("text") == expected and value["goal"].get("saving") is False
            and (conversation_id is None or value["goal"].get("conversation_id") == conversation_id),
            "goal reflects the selected conversation after storage completion",
        )
        self.check(state["goal"]["source"] == "user", "the goal records user intent")
        return state["goal"]["conversation_id"]

    def resume(self, front, conversation_id):
        self.conversation(front)
        sessions = self.observe("ssmcp.ui.history", {"action": "list"})["sessions"]
        selected = next(index for index, item in enumerate(sessions) if item["id"] == conversation_id)
        self.local(front, "/resume")
        self.wait_state(front, lambda value: value["view"] == "history", "the real history picker is open")
        front.send(HOME + DOWN * selected + ENTER)

    def sidecar(self, conversation_id):
        return self.history_root / (conversation_id + ".goal.json")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        cache_before = self.seed_cache()
        front = self.all_tty("goal-startup-all-tty")
        self.check(front.process.stdout is None, "the startup positive case has a TTY on stdout")
        self.capture(front, "startup-cached-release-120", RELEASE + " available")
        self.check(front.input_bytes == 0, "startup notice appeared without typing or a UI observer call")
        self.check(self.cache.read_bytes() == cache_before, "a fresh release cache is read without rewriting it")
        self.no_discovery("the cached startup notice made no request through the external proxy trap")
        front.resize(ROWS, 48)
        front.pump(0.15)
        self.capture(front, "startup-cached-release-48", RELEASE + " available", 48)
        front.resize(ROWS, 120)
        self.oauth_login(front.credentials)

        draft = "DRAFT 保留 café"
        self.replace_short(front, draft)
        before = self.ui("/display/conversation")["draft"]
        front.resize(ROWS, 48)
        shot = front.until(lambda value: draft in text(value), "the Unicode draft on a narrow terminal")
        self.check(RELEASE + " available" not in text(shot), "the optional startup notice yields to the real input draft")
        self.check(self.ui("/display/conversation")["draft"] == before,
                   "startup and resize preserve the Unicode draft and byte cursor")
        self.replace_short(front, "")
        front.resize(ROWS, 120)

        self.local(front, "/goal set " + GOAL_A)
        first = self.goal(front, GOAL_A)
        first_file = self.history_root / (first + ".jsonl")
        first_before = first_file.read_bytes()
        sidecar = self.sidecar(first)
        first_goal = sidecar.read_bytes()
        value = json.loads(first_goal)
        self.check(value == {"schema_version": 1, "id": first, "workspace": str(self.workspace), "text": GOAL_A},
                   "the goal sidecar binds the exact conversation, workspace and Unicode text")
        self.check(stat.S_IMODE(sidecar.stat().st_mode) == 0o600, "the goal sidecar remains private")
        self.check(not self.observe("ssmcp.ui.history", {"action": "read", "id": first})["messages"],
                   "setting a goal creates no fabricated conversation message")
        self.capture_widths(front, "goal-a-set", GOAL_A)

        self.local(front, "/new")
        self.goal(front, GOAL_A, first)
        self.check(self.sidecar(first).read_bytes() == first_goal,
                   "a new canvas retains its current history and goal")
        front.disconnect()
        front.close()

        # A new canvas keeps the selected history. A separate, fresh process
        # creates B without resuming A, using the same private history directory.
        # Its stdout pipe also proves that the startup probe itself is skipped:
        # an expired cache would otherwise trigger the proxy and be rewritten.
        expired_before = self.seed_cache(expired=True)
        front = self.frontend("goal-startup-pipe", self.config("goal-startup-pipe"))
        self.oauth_login(front.credentials)
        self.check(front.process.stdout is not None, "the negative startup case retains machine-readable stdout")
        deadline = time.monotonic() + 4.25
        while time.monotonic() < deadline:
            front.pump(0.05)
        self.check(self.cache.read_bytes() == expired_before, "a stdout-pipe session leaves an expired cache untouched")
        self.no_discovery("the stdout-pipe session made no startup release request")
        self.check(RELEASE + " available" not in text(front.snapshot()),
                   "the stdout-pipe session did not display a synthetic release offer")
        self.local(front, "/goal " + GOAL_B)
        second = self.goal(front, GOAL_B)
        self.check(first != second, "new conversation goal has its own history identity")
        second_before = (self.history_root / (second + ".jsonl")).read_bytes()
        second_goal = self.sidecar(second).read_bytes()
        self.resume(front, first)
        self.goal(front, GOAL_A, first)
        self.local(front, "/goal show")
        self.capture(front, "goal-a-restored", GOAL_A)
        self.local(front, "/goal clear")
        self.goal(front, None, first)
        self.check(not self.sidecar(first).exists(), "clearing the current goal removes its sidecar")
        self.check(self.sidecar(second).read_bytes() == second_goal,
                   "clearing A leaves B's stored goal unchanged")
        self.check(first_file.read_bytes() == first_before, "goal viewing and clearing do not append to the message log")
        self.check(not self.queue()["queue"], "local goal commands do not enqueue agent work")
        front.disconnect()
        front.close()

        resumed = self.frontend("goal-reopened-pipe", self.config("goal-reopened-pipe"))
        self.oauth_login(resumed.credentials)
        self.resume(resumed, second)
        self.goal(resumed, GOAL_B, second)
        self.local(resumed, "/goal")
        self.capture_widths(resumed, "goal-b-after-process-restart", GOAL_B)
        self.check(self.sidecar(second).read_bytes() == second_goal,
                   "reopening a conversation reads its original persistent goal")
        self.check((self.history_root / (second + ".jsonl")).read_bytes() == second_before,
                   "reopening and showing the goal do not replay or duplicate messages")
        self.local(resumed, "/goal clear")
        self.goal(resumed, None, second)
        self.check(not self.sidecar(second).exists(), "goal clear also persists in the reopened process")
        self.check(not self.queue()["queue"], "reopening a goal does not restore old queued work")
        self.check(self.cache.read_bytes() == expired_before, "reopened pipe-output sessions still leave discovery disabled")
        self.no_discovery("all goal scenarios kept external release discovery suppressed")
        self.check(all(not self.external_invocations(label) for label in self.labels),
                   "goal and startup scenarios started no external model or harness")
        resumed.disconnect()
        resumed.close()
        return {"status": "passed", "checks": self.checks, "frames": self.frames,
                "frame_manifest": self.frame_manifest,
                "fixture_release_cache": {"synthetic": True, "version": RELEASE, "target": self.target},
                "proxy_connections": self.proxy_connections, "conversations": [first, second],
                "models_started": False, "existing_services_touched": False}

    def cleanup(self):
        try:
            self.close_frontends()
        finally:
            self.proxy.close()
            super().cleanup()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--decoder", required=True, type=Path)
    parser.add_argument("--fixture-binary", type=Path, default=os.environ.get("SSMCP_E2E_FIXTURE_BINARY"))
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        parser.error("--source-revision must be the complete frozen source SHA")
    if args.fixture_binary is None:
        parser.error("provide the prebuilt library fixture through --fixture-binary or SSMCP_E2E_FIXTURE_BINARY")
    binaries = {name: args.bin_dir.resolve() / name for name in ("ssmcp", "ssmcpd")}
    binaries.update(decoder=args.decoder.resolve(), fixture=args.fixture_binary.resolve())
    if any(not path.is_file() or not os.access(path, os.X_OK) for path in binaries.values()):
        parser.error("all frozen executables must already exist; this fixture never builds")
    args.out.mkdir(parents=True, exist_ok=True)
    if any(args.out.iterdir()):
        parser.error("use a new empty output directory; existing evidence is never overwritten")
    os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(binaries["fixture"])
    before = {name: digest(path) for name, path in binaries.items()}
    started = time.monotonic()
    report = {"status": "failed", "checks": [], "frames": []}
    failure = None
    with tempfile.TemporaryDirectory(prefix="ssmcp-goal-startup-") as temporary:
        harness = GoalStartup(Path(temporary), args.bin_dir.resolve(), binaries["decoder"], args.out)

        def interrupted(_signal, _frame):
            raise TimeoutError("owned goal/startup fixture interrupted")

        signal.signal(signal.SIGTERM, interrupted)
        signal.signal(signal.SIGINT, interrupted)
        try:
            report = harness.run()
        except Exception as error:
            failure = f"{type(error).__name__}: {error}"
        finally:
            try:
                harness.cleanup()
            except Exception as error:
                failure = (failure + "; " if failure else "") + "cleanup: " + str(error)
        after = {name: digest(path) for name, path in binaries.items()}
        if before != after:
            failure = "a frozen executable changed during this fixture"
        report.update(checks=harness.checks, frames=harness.frames,
                      frame_manifest=harness.frame_manifest,
                      status="failed" if failure else report["status"],
                      source_revision=args.source_revision,
                      elapsed_seconds=round(time.monotonic() - started, 3),
                      binaries_sha256_before=before, binaries_sha256_after=after,
                      inputs_unchanged=before == after,
                      source_sha256={name: digest(Path(__file__).with_name(name)) for name in (
                          "e2e_goal_startup.py", "e2e_interactions.py", "e2e_input_history.py",
                          "e2e_composer.py", "e2e_dashboard.py", "e2e_client_ui.py",
                          "e2e_account_gateway.py", "e2e.py")})
        if failure:
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    failure = failure.replace(secret, "[redacted]")
            report["error"] = failure
        encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        if any(secret and secret in encoded for secret in [harness.secret, *harness.sensitive]):
            raise AssertionError("the report contains a private fixture credential")
        (args.out / "report.json").write_text(encoded, encoding="utf-8")
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "frames": len(report["frames"]), "elapsed_seconds": report["elapsed_seconds"]}))
    if failure:
        raise SystemExit(failure)


if __name__ == "__main__":
    main()
