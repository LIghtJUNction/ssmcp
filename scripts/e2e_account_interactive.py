#!/usr/bin/env python3
"""Exercise the real CLI's -i UI against a loopback account API and real PTY shell.

No GitHub account, SSH server, model, or production service is contacted.
"""
import argparse
import base64
import errno
import fcntl
import hashlib
import http.server
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import tempfile
import termios
import threading
import time


def child_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


def spawn_terminal(argv, env, cwd):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    child = subprocess.Popen(argv, stdin=slave, stdout=slave, stderr=slave,
                             env=env, cwd=cwd, preexec_fn=child_terminal)
    os.close(slave)
    return child, master


def stop(child, master):
    if child is not None and child.poll() is None:
        try:
            os.killpg(child.pid, signal.SIGTERM)
            child.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(child.pid, signal.SIGKILL)
            child.wait(timeout=3)
    if master is not None:
        os.close(master)


class Backend:
    def __init__(self, directory):
        self.condition = threading.Condition()
        self.output = bytearray()
        self.inputs = []
        self.sizes = []
        self.eof = False
        self.child, self.master = spawn_terminal(
            ["/bin/sh"], dict(os.environ, PS1="fixture-b> ", ENV=""),
            directory)
        self.reader = threading.Thread(target=self.capture, daemon=True)
        self.reader.start()

    def capture(self):
        try:
            while True:
                ready, _, _ = select.select([self.master], [], [], 0.2)
                if not ready:
                    if self.child.poll() is not None:
                        break
                    continue
                try:
                    data = os.read(self.master, 16384)
                except OSError as error:
                    if error.errno == errno.EIO:
                        break
                    raise
                if not data:
                    break
                with self.condition:
                    self.output.extend(data)
                    if len(self.output) > 1024 * 1024:
                        raise RuntimeError("fixture shell exceeded its output budget")
                    self.condition.notify_all()
        finally:
            with self.condition:
                self.eof = True
                self.condition.notify_all()

    def call(self, name, args):
        if args.get("terminal_id", "main") != "main":
            raise ValueError("unknown fixture terminal")
        if name == "terminal.status":
            with self.condition:
                return dict(terminal_id="main", running=self.child.poll() is None,
                            next_cursor=len(self.output), output_eof=self.eof)
        if name == "terminal.read":
            cursor = args.get("cursor", 0)
            limit = args.get("max_bytes", 16384)
            deadline = time.monotonic() + min(args.get("wait_ms", 0), 5000) / 1000
            with self.condition:
                while cursor == len(self.output) and not self.eof:
                    remaining = deadline - time.monotonic()
                    if remaining <= 0:
                        break
                    self.condition.wait(remaining)
                if cursor < 0 or cursor > len(self.output):
                    raise ValueError("invalid cursor")
                data = bytes(self.output[cursor:cursor + limit])
                return dict(terminal_id="main", data_base64=base64.b64encode(data).decode(),
                            text=data.decode(errors="replace"), next_cursor=cursor + len(data),
                            output_eof=self.eof, truncated=False)
        if name == "terminal.write":
            data = args["data"].encode()
            self.inputs.append(data)
            written = os.write(self.master, data)
            return dict(terminal_id="main", written=written)
        if name == "terminal.resize":
            rows, cols = args["rows"], args["cols"]
            self.sizes.append((rows, cols))
            fcntl.ioctl(self.master, termios.TIOCSWINSZ,
                        struct.pack("HHHH", rows, cols, 0, 0))
            return dict(terminal_id="main", rows=rows, cols=cols)
        raise ValueError(f"unexpected tool: {name}")


def fixture_handler(backend, token, cwd):
    class Handler(http.server.BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, *_):
            pass

        def reply(self, status, value):
            body = json.dumps(value).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            try:
                self.wfile.write(body)
            except (BrokenPipeError, ConnectionResetError):
                pass

        def authorized(self):
            if self.headers.get("Authorization") != f"Bearer {token}":
                self.reply(401, {"error": "invalid_token"})
                return False
            return True

        def do_GET(self):
            if not self.authorized():
                return
            if self.path == "/api/client/sessions":
                self.reply(200, {"clients": [{
                    "id": "fixture-b-session", "name": "fixture-b", "type": "shell",
                    "hostname": "loopback", "cwd": str(cwd),
                }]})
            else:
                self.reply(404, {"error": "not_found"})

        def do_POST(self):
            size = int(self.headers.get("Content-Length", "0"))
            body = json.loads(self.rfile.read(size))
            if not self.authorized():
                return
            if self.path != "/api/client/call" or body.get("client_id") != "fixture-b-session":
                self.reply(404, {"error": "not_found"})
                return
            try:
                result = backend.call(body["name"], body.get("arguments", {}))
                self.reply(200, {"structuredContent": result,
                                 "content": [{"type": "text", "text": json.dumps(result)}],
                                 "isError": False})
            except Exception as error:
                self.reply(400, {"error": type(error).__name__})
    return Handler


class TerminalOutput:
    def __init__(self, process, master):
        self.process = process
        self.master = master
        self.output = bytearray()

    def until(self, needle, timeout=12):
        needle = needle.encode()
        deadline = time.monotonic() + timeout
        while needle not in self.output:
            if self.process.poll() is not None:
                raise AssertionError(f"CLI exited {self.process.returncode} before {needle!r}: "
                                     f"{self.output.decode(errors='replace')[-4000:]}")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise AssertionError(f"no {needle!r} in CLI output: "
                                     f"{self.output.decode(errors='replace')[-4000:]}")
            ready, _, _ = select.select([self.master], [], [], min(remaining, 0.2))
            if ready:
                try:
                    data = os.read(self.master, 16384)
                except OSError as error:
                    if error.errno == errno.EIO:
                        raise AssertionError("CLI terminal closed unexpectedly") from error
                    raise
                self.output.extend(data)
                if len(self.output) > 1024 * 1024:
                    raise AssertionError("CLI output exceeded fixture budget")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin-dir", default="target/debug")
    args = parser.parse_args()
    binary = (Path(args.bin_dir) / "ssmcp").resolve()
    if not binary.is_file():
        raise SystemExit(f"Build the client first: {binary}")
    process = master = backend = server = None
    with tempfile.TemporaryDirectory(prefix="ssmcp-account-interactive-") as temporary:
        root = Path(temporary)
        auth = root / "auth"
        auth.mkdir(mode=0o700)
        try:
            backend = Backend(root)
            token = "c" * 64  # Local deterministic fixture value; never a user credential.
            server = http.server.ThreadingHTTPServer(("127.0.0.1", 0),
                                                      fixture_handler(backend, token, root))
            server.daemon_threads = True
            threading.Thread(target=server.serve_forever, daemon=True).start()
            origin = f"http://127.0.0.1:{server.server_port}"
            credential = auth / (hashlib.sha256(origin.encode()).hexdigest() + ".json")
            credential.write_text(json.dumps(dict(server=origin, token=token,
                                                   principal="fixture-user@github",
                                                   expires_at=int(time.time()) + 600)))
            credential.chmod(0o600)
            environment = dict(os.environ, SSMCP_AUTH_HOME=str(auth), NO_COLOR="1",
                               TERM="xterm-256color")
            process, master = spawn_terminal([str(binary), "--server", origin, "-i"],
                                              environment, root)
            output = TerminalOutput(process, master)
            output.until("Session number, ID or name:")
            os.write(master, b"1\n")
            output.until("Connected to fixture-b-session")
            os.write(master, b"printf 'SSMCP_INTERACTIVE_%s\\n' OK\n")
            output.until("SSMCP_INTERACTIVE_OK")

            os.write(master, b"printf 'SSMCP_JOB_%s\\n' STARTED; sleep 30\n")
            output.until("SSMCP_JOB_STARTED")
            os.write(master, b"\x03")
            os.write(master, b"printf 'SSMCP_INTERRUPT_%s\\n' OK\n")
            output.until("SSMCP_INTERRUPT_OK", timeout=8)
            assert any(b"\x03" in data for data in backend.inputs), "Ctrl-C did not reach B"

            fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 37, 101, 0, 0))
            os.killpg(process.pid, signal.SIGWINCH)
            deadline = time.monotonic() + 5
            while (37, 101) not in backend.sizes:
                if time.monotonic() >= deadline:
                    raise AssertionError(f"resize was not forwarded: {backend.sizes}")
                time.sleep(0.02)
            os.write(master, b"stty size\n")
            output.until("37 101")

            os.write(master, b"\x1d")
            process.wait(timeout=6)
            assert process.returncode == 0, f"detach exit: {process.returncode}"
            assert backend.child.poll() is None, "detaching killed the controlled shell"
            assert all(b"\x1d" not in data for data in backend.inputs), "detach key reached B"
            print(json.dumps({
                "passed": True, "mode": "real_cli_and_real_pty_loopback_api",
                "checks": ["interactive_selection", "remote_shell_input_output",
                           "ctrl_c_interrupts_remote_job", "window_resize",
                           "ctrl_bracket_detaches", "remote_session_survives_detach"],
            }))
        finally:
            stop(process, master)
            if backend is not None:
                stop(backend.child, backend.master)
            if server is not None:
                server.shutdown()
                server.server_close()


if __name__ == "__main__":
    main()
