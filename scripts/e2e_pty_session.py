"""Shared PTY and account relay helpers for isolated acceptance fixtures.

Callers isolate the process before constructing Session. Extracted from
c0d33f3c9077ece692b0bc198f30017b6918a8bc:scripts/e2e_computer_controls.py;
retired desktop rendering and computer-control scenarios are not included.
"""
import errno
import fcntl
import json
import os
import select
import signal
import struct
import subprocess
import sys
import termios
import threading
import time

import e2e_account_relay
from e2e_dashboard import Decoder


def save(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
    temporary.replace(path)


class Session:
    def __init__(self, root, label, binary, decoder, report, capabilities=None):
        self.root = root / label
        self.root.mkdir()
        self.label, self.report = label, report
        bindir, workspace = self.root / "bin", self.root / "workspace"
        bindir.mkdir()
        workspace.mkdir()
        config = self.root / "config.toml"
        config.write_text("[ui]\ndashboard = true\n")
        self.state = e2e_account_relay.State(capabilities=capabilities)
        self.server = e2e_account_relay.ThreadingHTTPServer(("127.0.0.1", 0), e2e_account_relay.Handler)
        self.server.daemon_threads = True
        self.server.state = self.state
        self.state.origin = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.server_thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.server_thread.start()
        env = os.environ.copy()
        env.update({"PATH": str(bindir), "TERM": "xterm-256color",
                    "SSMCP_AUTH_HOME": str(self.root / "auth"),
                    "SSMCP_CLIENT_CONFIG": str(config), "SSMCP_UI_CONFIG": str(config)})
        self.decoder = Decoder(decoder)
        self.decoder.request(op="resize", rows=38, cols=120)
        self.master, slave = os.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 38, 120, 0, 0))
        os.set_blocking(self.master, False)
        # Acquire only this new PTY in the child, then replace that process with
        # the real candidate. Avoid preexec_fn in the threaded HTTP fixture.
        tty_bootstrap = ("import fcntl,os,sys,termios; os.setsid(); "
                         "fcntl.ioctl(0,termios.TIOCSCTTY,0); os.tcsetpgrp(0,os.getpgrp()); "
                         "os.execv(sys.argv[1],sys.argv[1:])")
        self.process = subprocess.Popen(
            [sys.executable, "-c", tty_bootstrap, str(binary),
             self.state.allow_principal,
             "--name", label, "--no-watchdog", "--no-setup", "--no-browser", "--shell", "/bin/sh",
             "--ui-config", str(config)],
            cwd=workspace, env=env, stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        self.transcript = (report / f"{label}.ansi").open("wb")
        self.screen = ""
        self.closed = False

    def pump(self):
        output = bytearray()
        while True:
            try:
                block = os.read(self.master, 65536)
                if not block:
                    break
                output.extend(block)
            except BlockingIOError:
                break
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                break
        if output:
            self.transcript.write(output)
            self.transcript.flush()
            self.decoder.feed(output)
            self.screen = "\n".join(self.decoder.request(op="snapshot", cells=False)["rows"])
        assert not self.state.errors, self.state.errors
        assert self.process.poll() is None, f"{self.label} exited: {self.screen[-3000:]}"

    def write(self, data):
        assert os.write(self.master, data) == len(data)

    def close(self):
        if self.closed:
            return
        self.closed = True
        (self.report / f"{self.label}-screen.txt").write_text(self.screen)
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=3)
        os.close(self.master)
        self.transcript.close()
        self.decoder.close()
        self.server.shutdown()
        self.server.server_close()
        self.server_thread.join(timeout=2)
        assert not self.state.errors, self.state.errors


def until(sessions, predicate, label, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for session in sessions:
            session.pump()
        result = predicate()
        if result:
            return result
        select.select([session.master for session in sessions], [], [], 0.02)
    raise AssertionError(f"timed out: {label}")
