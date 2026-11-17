#!/usr/bin/env python3
"""Exercise CLI first-run/cancellation using an owned Linux PTY and loopback portal.

The default five cases keep authorization pending. Optional SSH prompt cases
use a synthetic loopback account and disposable encrypted keys with local sshd.
Each CLI child uses a private fixture HOME/USERPROFILE with a 0700 .ssh;
-F selects a temporary SSH configuration. The caller environment is unchanged.
No real browser, GitHub account, CLI key generation,
package installation, local harness, or background ssmcp session is started.

The outer probe-deadline case still executes the real system OpenSSH. Its
fixture wrapper changes only ConnectTimeout=8 to ConnectTimeout=0 so the
product's 30-second probe timer can be observed before OpenSSH's own timer.
"""

from __future__ import annotations

import argparse
import base64
import errno
import fcntl
import hashlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import pty
import pwd
import select
import secrets
import shlex
import shutil
import signal
import socket
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import urllib.request


DEVICE_CODE = "F" * 43
USER_CODE = "TEST-1234"


class Portal:
    """Pending by default; only explicit local-SSH fixtures supply a synthetic account."""

    def __init__(self, account=None):
        self.account = account
        self.events = []
        self.lock = threading.Lock()
        portal = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *_args):
                pass

            def do_POST(self):
                try:
                    size = int(self.headers.get("Content-Length", "0"))
                    if not 0 <= size <= 4096:
                        raise ValueError("request body exceeds fixture limit")
                    body = json.loads(self.rfile.read(size))
                    path = self.path
                    valid = (
                        path == "/api/setup/start"
                        and isinstance(body.get("client_name"), str)
                    ) or (
                        path in ("/api/setup/poll", "/api/setup/cancel")
                        and body.get("device_code") == DEVICE_CODE
                    )
                    with portal.lock:
                        portal.events.append({
                            "path": path,
                            "valid": valid,
                            "time": time.monotonic(),
                        })
                    if not valid:
                        self.reply(400, {"error": "unexpected_fixture_request"})
                    elif path == "/api/setup/start":
                        self.reply(200, {
                            "device_code": DEVICE_CODE,
                            "user_code": USER_CODE,
                            "verification_uri_complete": portal.verification_url,
                            "expires_in": 120,
                            "interval": 1,
                        })
                    elif path == "/api/setup/poll":
                        self.reply(200, {"status": "authorized", "account": portal.account}
                                   if portal.account else {"status": "pending"})
                    else:
                        self.reply(200, {"status": "cancelled"})
                except (ValueError, TypeError, json.JSONDecodeError) as error:
                    with portal.lock:
                        portal.events.append({"path": self.path, "valid": False,
                                              "error": str(error)})
                    self.reply(400, {"error": "invalid_fixture_request"})

            def do_GET(self):
                with portal.lock:
                    portal.events.append({"path": self.path, "valid": False,
                                          "error": "real browser navigation is forbidden"})
                self.reply(404, {"error": "fixture_has_no_browser"})

            def reply(self, status, body):
                data = json.dumps(body).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.send_header("Connection", "close")
                self.end_headers()
                try:
                    self.wfile.write(data)
                except (BrokenPipeError, ConnectionResetError):
                    pass
                self.close_connection = True

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.base = f"http://127.0.0.1:{self.server.server_port}"
        self.verification_url = f"{self.base}/setup?code={USER_CODE}"
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_args):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)

    def snapshot(self):
        with self.lock:
            return list(self.events)

    def routes(self):
        return [event["path"] for event in self.snapshot()]

    def assert_valid(self):
        assert all(event["valid"] for event in self.snapshot()), self.snapshot()


class KeptChild:
    """Observe/signal the CLI while a separate session leader retains its PTY."""

    def __init__(self, keeper, identity_path, exit_path):
        self.keeper = keeper
        self.identity_path = identity_path
        self.exit_path = exit_path
        self.returncode = None

    def identity(self):
        try:
            return json.loads(self.identity_path.read_text())
        except (FileNotFoundError, json.JSONDecodeError):
            return None

    def poll(self):
        if self.returncode is None:
            try:
                self.returncode = json.loads(self.exit_path.read_text())["returncode"]
            except (FileNotFoundError, json.JSONDecodeError):
                if self.keeper.poll() is not None:
                    raise AssertionError("PTY keeper exited before recording the CLI status")
        return self.returncode

    def send_signal(self, number):
        identity = self.identity()
        if not identity or not alive(identity):
            return
        descriptor = None
        try:
            descriptor = os.pidfd_open(identity["pid"])
            if alive(identity):
                signal.pidfd_send_signal(descriptor, number)
        except ProcessLookupError:
            pass
        finally:
            if descriptor is not None:
                os.close(descriptor)

    def terminate(self):
        self.send_signal(signal.SIGTERM)

    def kill(self):
        self.send_signal(signal.SIGKILL)

    def wait(self, timeout):
        deadline = time.monotonic() + timeout
        while self.poll() is None:
            if time.monotonic() >= deadline:
                raise subprocess.TimeoutExpired("fixture CLI", timeout)
            time.sleep(0.02)
        return self.returncode


class PtyProcess:
    """Keep the session leader alive after CLI exit so SIGHUP cannot hide leaks."""

    def __init__(self, command, *, cwd, env):
        self.master, slave = pty.openpty()
        release_read, self.release_write = os.pipe()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        identity_path = Path(cwd) / ".pty-cli-identity.json"
        exit_path = Path(cwd) / ".pty-cli-exit.json"
        launcher = [
            sys.executable, str(Path(__file__).resolve()), "--exec-pty-keeper",
            str(release_read), str(identity_path), str(exit_path), *command,
        ]
        try:
            self.keeper = subprocess.Popen(
                launcher, cwd=cwd, env=env, stdin=slave, stdout=slave, stderr=slave,
                start_new_session=True, pass_fds=(release_read,), close_fds=True,
            )
        finally:
            os.close(slave)
            os.close(release_read)
        self.process = KeptChild(self.keeper, identity_path, exit_path)
        os.set_blocking(self.master, False)
        self.output = bytearray()
        self.started = time.monotonic()
        self.closed = False

    def pump(self, seconds=0.05):
        if self.closed:
            return
        ready, _, _ = select.select([self.master], [], [], seconds)
        if not ready:
            return
        try:
            chunk = os.read(self.master, 65536)
        except OSError as error:
            if error.errno in (errno.EIO, errno.EAGAIN, errno.EWOULDBLOCK):
                return
            raise
        self.output.extend(chunk)
        if len(self.output) > 1024 * 1024:
            raise AssertionError("fixture CLI output exceeded 1 MiB")

    def text(self):
        return self.output.decode(errors="replace")

    def wait_for(self, condition, timeout, label):
        deadline = time.monotonic() + timeout
        while True:
            self.pump()
            if condition():
                return
            if self.process.poll() is not None:
                self.pump(0)
                raise AssertionError(
                    f"CLI exited before {label}: {self.process.returncode}\n"
                    + self.text()[-6000:]
                )
            if time.monotonic() >= deadline:
                raise AssertionError(f"timed out waiting for {label}\n" + self.text()[-6000:])

    def wait_exit(self, timeout=8):
        deadline = time.monotonic() + timeout
        while self.process.poll() is None:
            self.pump()
            if time.monotonic() >= deadline:
                raise AssertionError("CLI did not exit after cancellation\n" + self.text()[-6000:])
        self.pump(0)
        assert self.keeper.poll() is None, "session keeper exited before cleanup observation"
        return self.process.returncode

    def ctrl_c(self):
        os.write(self.master, b"\x03")

    def terminate_cli_only(self):
        # Signal only ssmcp. Neither the live session keeper nor the foreground
        # process group receives this signal.
        self.process.send_signal(signal.SIGTERM)

    def close(self):
        if self.closed:
            return
        try:
            if self.process.poll() is None:
                self.process.terminate()
                try:
                    self.process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    self.process.kill()
                    self.process.wait(timeout=3)
        finally:
            # Only after observations may the session leader exit and its
            # controlling terminal be released.
            try:
                os.write(self.release_write, b"1")
            except BrokenPipeError:
                pass
            os.close(self.release_write)
            try:
                self.keeper.wait(timeout=3)
            except subprocess.TimeoutExpired:
                self.keeper.terminate()
                try:
                    self.keeper.wait(timeout=2)
                except subprocess.TimeoutExpired:
                    self.keeper.kill()
                    self.keeper.wait(timeout=2)
            os.close(self.master)
            self.closed = True

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        self.close()


def process_identity(pid):
    """A PID plus Linux start time distinguishes a fixture process from reuse."""
    try:
        text = Path(f"/proc/{pid}/stat").read_text()
        fields = text[text.rfind(")") + 2:].split()
        return {"pid": pid, "start": fields[19], "state": fields[0],
                "ppid": int(fields[1]), "pgrp": int(fields[2]),
                "session": int(fields[3]), "foreground_pgrp": int(fields[5])}
    except (FileNotFoundError, ProcessLookupError):
        return None


def process_snapshot(identity):
    current = process_identity(identity["pid"])
    if not current or current["start"] != identity["start"]:
        return {"pid": identity["pid"], "alive": False}
    current["alive"] = current["state"] != "Z"
    try:
        status = Path(f"/proc/{identity['pid']}/status").read_text().splitlines()
        current["signals"] = {line.split(":", 1)[0]: line.split(":", 1)[1].strip()
                              for line in status
                              if line.startswith(("SigBlk:", "SigIgn:", "SigCgt:"))}
    except FileNotFoundError:
        pass
    return current


def alive(identity):
    current = process_identity(identity["pid"]) if identity else None
    return bool(current and current["start"] == identity["start"]
                and current["state"] != "Z")


def stop_owned(identity):
    if not alive(identity):
        return
    # pidfd binds cleanup to the exact observed process even if it exits here.
    descriptor = None
    try:
        descriptor = os.pidfd_open(identity["pid"])
        if not alive(identity):
            return
        signal.pidfd_send_signal(descriptor, signal.SIGTERM)
        deadline = time.monotonic() + 1
        while alive(identity) and time.monotonic() < deadline:
            time.sleep(0.02)
        if alive(identity):
            signal.pidfd_send_signal(descriptor, signal.SIGKILL)
    except ProcessLookupError:
        pass
    finally:
        if descriptor is not None:
            os.close(descriptor)


def write_script(path, code):
    path.write_text("#!" + sys.executable + "\n" + code, encoding="utf-8")
    path.chmod(0o700)


class Sandbox:
    def __init__(self, root, binary, ssh, portal, closed_port, *, proxy=False,
                 outer_deadline=False):
        self.root = root
        self.binary = binary
        self.portal = portal
        self.config = root / "empty SSH config"
        self.config.write_bytes(b"")
        self.bin = root / "bin"
        self.bin.mkdir()
        self.browser_log = root / "browser.jsonl"
        self.keygen_log = root / "unexpected-keygen"
        self.ssh_log = root / "ssh.jsonl"
        self.proxy_state = root / "proxy.json"
        self.proxy_signal = root / "proxy-signal.json"
        self.cleanup_snapshot = None
        self.home = root / "home"
        self.home.mkdir(mode=0o700)
        self.home_ssh = self.home / ".ssh"
        self.home_ssh.mkdir(mode=0o700)
        self.home_ssh_before = self.home_ssh.stat()
        self.parent_home_environment = {
            key: os.environ.get(key) for key in ("HOME", "USERPROFILE")
        }
        self.env = os.environ.copy()
        self.env["HOME"] = str(self.home)
        self.env["USERPROFILE"] = str(self.home)
        self.env["PATH"] = str(self.bin)
        self.env["TERM"] = "xterm-256color"
        self.env["NO_PROXY"] = "127.0.0.1,::1,localhost"
        self.env["no_proxy"] = self.env["NO_PROXY"]
        for name in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy",
                     "https_proxy", "all_proxy", "SSH_AUTH_SOCK", "SSH_ASKPASS",
                     "SSH_ASKPASS_REQUIRE"):
            self.env.pop(name, None)
        write_script(self.bin / "ssh", f"""import os, sys, json
from pathlib import Path
arguments = sys.argv[1:]
text=Path('/proc/self/stat').read_text()
fields=text[text.rfind(')')+2:].split()
with Path({str(self.ssh_log)!r}).open('a') as log:
    log.write(json.dumps({{'pid':os.getpid(),'start':fields[19], 'config_probe':'-G' in arguments}})+'\\n')
if {outer_deadline!r}:
    arguments = ['ConnectTimeout=0' if value == 'ConnectTimeout=8' else value
                 for value in arguments]
os.execv({str(ssh)!r}, [{str(ssh)!r}] + arguments)
""")
        # Discovery sees a complete tool pair; execution before authorization is
        # a test failure. This sentinel makes accidental key writes impossible.
        write_script(self.bin / "ssh-keygen", f"""from pathlib import Path
Path({str(self.keygen_log)!r}).write_text('unexpected keygen invocation')
raise SystemExit(99)
""")
        write_script(self.bin / "xdg-open", f"""import json, os, sys
from pathlib import Path
with Path({str(self.browser_log)!r}).open('a') as output:
    output.write(json.dumps({{'arguments':sys.argv[1:], 'pid':os.getpid()}})+'\\n')
raise SystemExit(0)
""")
        if proxy:
            helper = root / "proxy-helper.py"
            helper.write_text(f"""import json, os, signal, time
from pathlib import Path
def identity(pid):
    text=Path('/proc/'+str(pid)+'/stat').read_text()
    fields=text[text.rfind(')')+2:].split()
    return {{'pid':pid,'start':fields[19],'state':fields[0]}}
def stopped(number, frame):
    Path({str(self.proxy_signal)!r}).write_text(json.dumps({{'signal':number}}))
    raise SystemExit(0)
signal.signal(signal.SIGTERM, stopped)
signal.signal(signal.SIGHUP, stopped)
terminal=False
try:
    with open('/dev/tty'): terminal=True
except OSError:
    pass
Path({str(self.proxy_state)!r}).write_text(json.dumps({{
    'proxy':identity(os.getpid()), 'ssh':identity(os.getppid()),
    'controlling_terminal':terminal
}}))
while True: time.sleep(1)
""", encoding="utf-8")
            proxy_command = shlex.join([sys.executable, str(helper)])
            self.config.write_text(
                "Host *\n"
                "  HostName 127.0.0.1\n"
                f"  ProxyCommand {proxy_command}\n"
                "  StrictHostKeyChecking yes\n"
                "  UserKnownHostsFile /dev/null\n"
                "  GlobalKnownHostsFile /dev/null\n"
                "  IdentityFile none\n"
                "  IdentitiesOnly yes\n",
                encoding="utf-8",
            )
        self.original_config = self.config.read_bytes()
        self.command = [
            str(binary), "--no-color", "--no-watchdog", "--name", "onboarding-fixture",
            "-F", str(self.config), "-p", str(closed_port),
            "--portal", portal.base, "127.0.0.1",
        ]

    def opened(self):
        if not self.browser_log.exists():
            return []
        return [json.loads(line) for line in self.browser_log.read_text().splitlines()]

    def assert_private_home(self):
        assert self.env["HOME"] == str(self.home)
        assert self.env["USERPROFILE"] == str(self.home)
        assert all(os.environ.get(key) == value
                   for key, value in self.parent_home_environment.items()), "caller home environment changed"
        before = self.home_ssh_before
        after = self.home_ssh.stat()
        assert (after.st_ino, after.st_mode, after.st_mtime_ns) == (
            before.st_ino, before.st_mode, before.st_mtime_ns), "fixture SSH directory changed"
        assert not any(self.home_ssh.iterdir()), "unexpected files in fixture home/.ssh"

    def assert_untouched(self):
        assert self.config.read_bytes() == self.original_config, "fixture SSH config changed"
        assert not self.keygen_log.exists(), "ssh-keygen ran before authorization"
        self.assert_private_home()
        self.portal.assert_valid()

    def proxy(self):
        if not self.proxy_state.exists():
            return None
        try:
            return json.loads(self.proxy_state.read_text())
        except json.JSONDecodeError:
            return None

    def cleanup_proxy(self):
        state = self.proxy()
        if state:
            stop_owned(state["ssh"])
            stop_owned(state["proxy"])

    def result(self, name, process=None, **extra):
        self.assert_untouched()
        value = {
            "name": name,
            "portal_routes": self.portal.routes(),
            "browser_requests": self.opened(),
            "ssh_config_unchanged": True,
            "keygen_unused": True,
            "home_isolated": True,
            "caller_home_environment_unchanged": True,
            **extra,
        }
        if process:
            value.update({
                "exit_code": process.process.returncode,
                "elapsed_seconds": round(time.monotonic() - process.started, 3),
                "output_tail": process.text()[-4000:],
            })
        return value



class SshServer:
    """An unprivileged loopback sshd with disposable host and encrypted user keys."""

    def __init__(self, root, ssh):
        self.root = root
        self.root.mkdir()
        self.ssh = ssh
        self.sshd = shutil.which("sshd", path="/usr/sbin:/usr/bin:/sbin:/bin")
        self.keygen = shutil.which("ssh-keygen", path="/usr/bin:/bin")
        if not self.sshd or not self.keygen:
            raise RuntimeError("real SSH prompt fixtures require installed sshd and ssh-keygen")
        self.user = pwd.getpwuid(os.getuid()).pw_name
        self.phrase = "fixture secret " + secrets.token_hex(16)
        self.identity = root / "encrypted_identity"
        self.host = root / "host_identity"
        self.authenticated = root / "authenticated.json"
        for path, phrase in [(self.host, ""), (self.identity, self.phrase)]:
            result = subprocess.run(
                [self.keygen, "-q", "-t", "ed25519", "-N", phrase, "-f", str(path)],
                stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=12, text=True,
            )
            if result.returncode:
                raise RuntimeError("temporary ssh-keygen failed: " + result.stderr)
        self.public = " ".join(Path(str(self.identity) + ".pub").read_text().split()[:2])
        self.fingerprint = self.fingerprint_of(self.public)
        host_public = " ".join(Path(str(self.host) + ".pub").read_text().split()[:2])
        self.host_public = host_public
        self.host_fingerprint = self.fingerprint_of(host_public)
        authorized = root / "authorized_keys"
        authorized.write_text(self.public + "\n")
        authorized.chmod(0o600)
        self.account = {"github_login": "onboarding-fixture", "username": self.user,
                        "keys": [{"public_key": self.public,
                                  "fingerprint": self.fingerprint}]}
        forced = root / "forced-command.py"
        forced.write_text(f"""import json,os,sys
from pathlib import Path
line=sys.stdin.buffer.readline(65537)
if line==b'SELFTEST\\n':
    print('SELFTEST_OK',flush=True)
else:
    value=json.loads(line)
    if value.get('type')!='hello': raise SystemExit(4)
    authenticated=Path({str(self.authenticated)!r})
    pending=authenticated.with_name(authenticated.name+'.'+str(os.getpid())+'.tmp')
    pending.write_text(json.dumps({{
        'received_hello':True,'protocol':value.get('version'),
        'name':value.get('metadata',{{}}).get('name'),
        'allow':value.get('metadata',{{}}).get('allow',[]),
        'pid':os.getpid()
    }}),encoding='utf-8')
    # Existence is the reader's completion signal: publish only finished JSON.
    os.replace(pending,authenticated)
    # Never send Ready: the test stops verification before local harnesses or a
    # background session could start. EOF makes server cleanup cooperative.
    while sys.stdin.buffer.read(4096): pass
""", encoding="utf-8")
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reservation:
            reservation.bind(("127.0.0.1", 0))
            self.port = reservation.getsockname()[1]
        self.config = root / "sshd.conf"
        self.config.write_text(
            f"ListenAddress 127.0.0.1\nPort {self.port}\n"
            f"HostKey {self.quote(self.host)}\n"
            f"PidFile {self.quote(root / 'sshd.pid')}\n"
            f"AuthorizedKeysFile {self.quote(authorized)}\n"
            f"AllowUsers {self.user}\n"
            "PubkeyAuthentication yes\nPasswordAuthentication no\n"
            "KbdInteractiveAuthentication no\nUsePAM no\nStrictModes no\n"
            "PermitRootLogin prohibit-password\nPermitUserRC no\n"
            "PermitUserEnvironment no\nDisableForwarding yes\n"
            "UseDNS no\nPrintMotd no\nLogLevel ERROR\n"
            f"ForceCommand {shlex.join([sys.executable, str(forced)])}\n",
            encoding="utf-8",
        )
        self.log_path = root / "sshd.log"
        self.log = self.log_path.open("wb")
        self.process = subprocess.Popen(
            [self.sshd, "-D", "-e", "-f", str(self.config)], stdin=subprocess.DEVNULL,
            stdout=self.log, stderr=self.log, start_new_session=True,
        )
        try:
            deadline = time.monotonic() + 5
            while True:
                if self.process.poll() is not None:
                    raise RuntimeError("temporary sshd exited: " + self.log_path.read_text()[-4000:])
                try:
                    with socket.create_connection(("127.0.0.1", self.port), timeout=0.1):
                        break
                except (ConnectionRefusedError, TimeoutError):
                    if time.monotonic() >= deadline:
                        raise RuntimeError("temporary loopback sshd did not start")
                    time.sleep(0.05)
            self.preflight(host_public)
        except Exception:
            self.close()
            raise

    @staticmethod
    def quote(path):
        return '"' + str(path).replace("\\", "\\\\").replace('"', '\\"') + '"'

    @staticmethod
    def fingerprint_of(public):
        digest = hashlib.sha256(base64.b64decode(public.split()[1])).digest()
        return "SHA256:" + base64.b64encode(digest).decode().rstrip("=")

    def preflight(self, host_public):
        # The bootstrap connection trusts only the host key generated above.
        # Product prompt cases use fresh empty known_hosts files instead.
        known = self.root / "preflight_known_hosts"
        known.write_text(f"[127.0.0.1]:{self.port} {host_public}\n")
        askpass = self.root / "preflight-askpass"
        write_script(askpass, "import os\nprint(os.environ['SSMCP_FIXTURE_PHRASE'])\n")
        environment = os.environ.copy()
        environment.pop("SSH_AUTH_SOCK", None)
        environment.update({
            "SSH_ASKPASS": str(askpass), "SSH_ASKPASS_REQUIRE": "force",
            "DISPLAY": "ssmcp-owned-fixture", "SSMCP_FIXTURE_PHRASE": self.phrase,
        })
        result = subprocess.run(
            [str(self.ssh), "-T", "-F", "/dev/null", "-p", str(self.port),
             "-i", str(self.identity), "-o", "IdentitiesOnly=yes",
             "-o", "StrictHostKeyChecking=yes", "-o", f"UserKnownHostsFile={self.quote(known)}",
             "-o", "GlobalKnownHostsFile=/dev/null", "-o", "ConnectTimeout=4",
             "--", f"{self.user}@127.0.0.1", "fixture"],
            input="SELFTEST\n", stdout=subprocess.PIPE, stderr=subprocess.PIPE,
            env=environment, timeout=15, text=True,
        )
        if result.returncode or result.stdout.strip() != "SELFTEST_OK":
            raise RuntimeError("temporary real-SSH preflight failed: "
                               + result.stderr.replace(self.phrase, "<redacted>")
                               + "\n" + self.log_path.read_text()[-4000:])
        assert self.phrase not in result.stdout + result.stderr
        self.initial_key_hash = hashlib.sha256(self.identity.read_bytes()).hexdigest()

    def close(self):
        if self.process.poll() is None:
            # This group belongs to our still-live, unreaped sshd leader.
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=3)
        self.log.close()

    def __enter__(self):
        return self

    def __exit__(self, *_args):
        self.close()


def prompt_cases(binary, root, ssh, results):
    # Sandbox supplies an owned private HOME/.ssh for complete(). This works
    # on fresh CI accounts without reading or creating the caller's ~/.ssh.
    for name in ("ssh_hostkey_cancel", "ssh_passphrase_cancel", "ssh_secret_authentication"):
        case_root = root / name
        case_root.mkdir()
        with SshServer(case_root / "server", ssh) as server, Portal(server.account) as portal:
            sandbox = Sandbox(case_root, binary, ssh, portal, server.port)
            known = case_root / "known_hosts"
            sandbox.config.write_text(
                "Host *\n"
                f"  UserKnownHostsFile {SshServer.quote(known)}\n"
                "  GlobalKnownHostsFile /dev/null\n"
                "  StrictHostKeyChecking ask\n"
                "  UpdateHostKeys no\n"
                "  VerifyHostKeyDNS no\n",
                encoding="utf-8",
            )
            command = sandbox.command.copy()
            command.insert(1, "setup")
            command[2:2] = ["-i", str(server.identity)]
            environment = sandbox.env.copy()
            environment["LC_ALL"] = "C"
            # An inherited confirmation hint must never make a secret echo.
            environment["SSH_ASKPASS_PROMPT"] = "confirm"
            process = None
            try:
                with PtyProcess(command, cwd=case_root, env=environment) as process:
                    initial_flags = termios.tcgetattr(process.master)[3]
                    restored_mask = termios.ECHO | termios.ICANON | termios.ISIG
                    process.wait_for(lambda: "给这台电脑取个名字" in process.text(), 12,
                                     "the local fixture account and default-name prompt")
                    os.write(process.master, b"\n")
                    process.wait_for(lambda: "continue connecting" in process.text(), 12,
                                     "OpenSSH's unknown-host confirmation")
                    assert server.host_fingerprint in process.text(), "host prompt has the wrong fixture fingerprint"
                    process.pump(0.15)
                    assert not known.exists() or known.stat().st_size == 0, "unknown host was automatically trusted"
                    assert not server.authenticated.exists()
                    assert not (termios.tcgetattr(process.master)[3] & termios.ECHO), "confirmation input is echoed"
                    if name != "ssh_hostkey_cancel":
                        answer_start = len(process.output)
                        os.write(process.master, b"yes\n")
                        process.wait_for(lambda: "enter passphrase for key" in process.text().lower(), 12,
                                         "OpenSSH's encrypted-key passphrase prompt")
                        echoed = bytes(process.output[answer_start:])
                        assert b"yes\r\n" not in echoed and b"yes\n" not in echoed, "host confirmation answer was echoed"
                        assert not (termios.tcgetattr(process.master)[3] & termios.ECHO), "secret input is echoed"
                        assert known.exists() and server.host_public.split()[1] in known.read_text()
                    if name == "ssh_secret_authentication":
                        os.write(process.master, server.phrase.encode() + b"\n")
                        process.wait_for(lambda: server.authenticated.exists(), 12,
                                         "real SSH authentication and registration Hello")
                        authenticated = json.loads(server.authenticated.read_text())
                        assert authenticated["received_hello"]
                        assert authenticated["name"] == "onboarding-fixture"
                        assert authenticated["allow"] == ["onboarding-fixture@github"]
                        assert server.phrase not in process.text(), "fixture passphrase was echoed"
                    process.terminate_cli_only()
                    assert process.wait_exit(8) == 0, process.text()
                    restored = termios.tcgetattr(process.master)[3]
                    assert restored & restored_mask == initial_flags & restored_mask, "TTY echo/canonical/ISIG modes were not restored"
                    assert process.keeper.poll() is None, "keeper exited before mode-restoration check"
                    ssh_processes = [json.loads(line) for line in sandbox.ssh_log.read_text().splitlines()]
                    assert all(not alive(item) for item in ssh_processes), "an SSH process survived prompt cancellation"
                    assert not sandbox.keygen_log.exists(), "CLI tried to generate a key"
                    assert hashlib.sha256(server.identity.read_bytes()).hexdigest() == server.initial_key_hash
                    assert portal.routes().count("/api/setup/cancel") == 1, portal.routes()
                    portal.assert_valid()
                    sandbox.assert_private_home()
                    results.append({
                        "name": name, "exit_code": process.process.returncode,
                        "elapsed_seconds": round(time.monotonic() - process.started, 3),
                        "real_openssh": str(ssh), "real_sshd": server.sshd,
                        "unknown_host_requires_answer": True,
                        "confirmation_and_secret_echo_disabled": True,
                        "terminal_modes_restored": True,
                        "session_keeper_alive_at_observation": True,
                        "private_key_unchanged": True, "home_isolated": True,
                        "caller_home_environment_unchanged": True,
                        "fixture_ssh_directory_unchanged": True,
                        "cli_keygen_unused": True, "portal_routes": portal.routes(),
                        "authenticated": name == "ssh_secret_authentication",
                        "output_tail": process.text()[-4000:].replace(server.phrase, "<redacted>"),
                    })
            except Exception as error:
                results.append({
                    "name": name, "status": "failed",
                    "error": str(error).replace(server.phrase, "<redacted>"),
                    "portal_routes": portal.routes(),
                    "output_tail": process.text()[-6000:].replace(server.phrase, "<redacted>") if process else "",
                    "sshd_tail": server.log_path.read_text()[-4000:].replace(server.phrase, "<redacted>"),
                })
                raise AssertionError(str(error).replace(server.phrase, "<redacted>")) from None
            finally:
                if sandbox.ssh_log.exists():
                    for line in sandbox.ssh_log.read_text().splitlines():
                        stop_owned(json.loads(line))
        print(f"PASS {name}", flush=True)


def wait_pending(process, sandbox, timeout=15):
    process.wait_for(
        lambda: sandbox.portal.routes().count("/api/setup/start") == 1
        and "/api/setup/poll" in sandbox.portal.routes()
        and bool(sandbox.opened()),
        timeout, "automatic setup, browser request, and pending poll",
    )
    assert sandbox.opened() == [{
        "arguments": [sandbox.portal.verification_url],
        "pid": sandbox.opened()[0]["pid"],
    }], sandbox.opened()


def cancel_pending(process, sandbox):
    process.ctrl_c()
    assert process.wait_exit() == 0, process.text()
    assert sandbox.portal.routes().count("/api/setup/cancel") == 1, sandbox.portal.routes()


def main_cases(binary, root, ssh, closed_port, results):
    for name in ("auto_setup_ctrl_c", "no_setup_tty", "non_tty",
                 "probe_term_proxy_cleanup", "probe_deadline_proxy_cleanup"):
        case_root = root / name
        case_root.mkdir()
        with Portal() as portal:
            proxy = name.startswith("probe_")
            sandbox = Sandbox(case_root, binary, ssh, portal, closed_port,
                              proxy=proxy, outer_deadline=name == "probe_deadline_proxy_cleanup")
            process = None
            try:
                if name == "non_tty":
                    result = subprocess.run(
                        sandbox.command, cwd=case_root, env=sandbox.env,
                        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                        stderr=subprocess.PIPE, timeout=15, text=True,
                    )
                    assert result.returncode != 0, result.stdout + result.stderr
                    assert not portal.routes(), portal.routes()
                    assert not sandbox.opened(), sandbox.opened()
                    results.append(sandbox.result(
                        name, exit_code=result.returncode,
                        output_tail=(result.stdout + result.stderr)[-4000:],
                    ))
                    print(f"PASS {name}", flush=True)
                    continue
                command = sandbox.command.copy()
                if name == "no_setup_tty":
                    command.insert(1, "--no-setup")
                elif proxy:
                    command.insert(1, "--daemon")
                with PtyProcess(command, cwd=case_root, env=sandbox.env) as process:
                    if name == "auto_setup_ctrl_c":
                        wait_pending(process, sandbox)
                        assert "SSH" in process.text(), process.text()
                        cancel_pending(process, sandbox)
                    elif name == "no_setup_tty":
                        assert process.wait_exit(15) != 0, process.text()
                        assert not portal.routes(), portal.routes()
                        assert not sandbox.opened(), sandbox.opened()
                    else:
                        process.wait_for(lambda: sandbox.proxy() is not None, 8,
                                         "the real SSH ProxyCommand")
                        state = sandbox.proxy()
                        assert not state["controlling_terminal"], "owned SSH children must be isolated from the terminal"
                        assert alive(state["proxy"]) and alive(state["ssh"]), state
                        if name == "probe_term_proxy_cleanup":
                            process.terminate_cli_only()
                            assert process.wait_exit() == 0, process.text()
                            assert not portal.routes(), portal.routes()
                            assert not sandbox.opened(), sandbox.opened()
                        else:
                            wait_pending(process, sandbox, timeout=40)
                            assert "SSH registration reached its deadline" in process.text(), process.text()
                            elapsed = time.monotonic() - process.started
                            assert 28 <= elapsed < 43, elapsed
                        # Check before closing the PTY. Closing it would deliver SIGHUP
                        # and could hide a leaked SSH or ProxyCommand process.
                        deadline = time.monotonic() + 2
                        while (alive(state["proxy"]) or alive(state["ssh"])) and time.monotonic() < deadline:
                            process.pump()
                        sandbox.cleanup_snapshot = {
                            "session_keeper_alive": process.keeper.poll() is None,
                            "cli": process_snapshot(process.process.identity()),
                            "ssh": process_snapshot(state["ssh"]),
                            "proxy": process_snapshot(state["proxy"]),
                            "proxy_signal": json.loads(sandbox.proxy_signal.read_text())
                                if sandbox.proxy_signal.exists() else None,
                        }
                        assert sandbox.cleanup_snapshot["session_keeper_alive"]
                        assert not alive(state["proxy"]), "ProxyCommand survived probe cleanup"
                        assert not alive(state["ssh"]), "OpenSSH survived probe cleanup"
                        if name == "probe_deadline_proxy_cleanup":
                            cancel_pending(process, sandbox)
                    extra = {}
                    if proxy:
                        extra = {
                            "real_openssh": str(ssh),
                            "cleanup_snapshot": sandbox.cleanup_snapshot,
                            "session_keeper_alive_at_cleanup_observation": True,
                            "proxy_and_ssh_stopped_before_pty_close": True,
                            "ssh_child_controlling_terminal": state["controlling_terminal"],
                            "cli_controlling_terminal_preserved": True,
                            "transport_timeout_disabled_to_exercise_outer_timer":
                                name == "probe_deadline_proxy_cleanup",
                            "proxy_signal": json.loads(sandbox.proxy_signal.read_text())
                                if sandbox.proxy_signal.exists() else None,
                        }
                    results.append(sandbox.result(name, process, **extra))
            except Exception as error:
                results.append({
                    "name": name, "status": "failed", "error": str(error),
                    "portal_routes": portal.routes(),
                    "browser_requests": sandbox.opened(),
                    "proxy": sandbox.proxy(),
                    "cleanup_snapshot": sandbox.cleanup_snapshot,
                    "output_tail": process.text()[-6000:] if process else "",
                })
                raise
            finally:
                sandbox.cleanup_proxy()
        print(f"PASS {name}", flush=True)
    return results


def self_test(root):
    # Verify the fixture's terminal and network behavior before relying on CLI assertions.
    code = (
        "import os,signal,sys;"
        "assert os.isatty(0) and os.isatty(2);"
        "open('/dev/tty').close();"
        "signal.signal(signal.SIGINT,lambda *_:sys.exit(0));"
        "print('PTY_READY',flush=True);signal.pause()"
    )
    with PtyProcess([sys.executable, "-c", code], cwd=root,
                    env=os.environ.copy()) as process:
        process.wait_for(lambda: "PTY_READY" in process.text(), 5, "fixture PTY readiness")
        process.ctrl_c()
        assert process.wait_exit(5) == 0, process.text()
        assert process.keeper.poll() is None, "session keeper must survive the CLI"
    # Session-leader exit can kill foreground descendants even while the PTY
    # master is open. Prove our keeper prevents that false positive.
    survivor_root = root / "keeper-lifetime-self-test"
    survivor_root.mkdir()
    survivor_file = survivor_root / "survivor.json"
    survivor_code = f"""import json,subprocess,sys
from pathlib import Path
child=subprocess.Popen([sys.executable,'-c','import time;time.sleep(60)'],
    stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
text=Path('/proc/'+str(child.pid)+'/stat').read_text()
fields=text[text.rfind(')')+2:].split()
Path({str(survivor_file)!r}).write_text(json.dumps({{'pid':child.pid,'start':fields[19]}}))
"""
    with PtyProcess([sys.executable, "-c", survivor_code], cwd=survivor_root,
                    env=os.environ.copy()) as process:
        identity = None
        try:
            assert process.wait_exit(5) == 0, process.text()
            identity = json.loads(survivor_file.read_text())
            time.sleep(0.1)
            assert process.keeper.poll() is None, "keeper did not survive CLI exit"
            assert alive(identity), "session exit incorrectly killed the survivor fixture"
        finally:
            if identity:
                stop_owned(identity)
    with Portal() as portal:
        opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
        for route, body in [
            ("start", {"client_name": "self-test"}),
            ("poll", {"device_code": DEVICE_CODE}),
            ("cancel", {"device_code": DEVICE_CODE}),
        ]:
            request = urllib.request.Request(
                f"{portal.base}/api/setup/{route}", data=json.dumps(body).encode(),
                headers={"Content-Type": "application/json"}, method="POST",
            )
            with opener.open(request, timeout=2) as response:
                value = json.load(response)
            if route == "poll":
                assert value["status"] == "pending", value
        portal.assert_valid()
        assert portal.routes() == [f"/api/setup/{x}" for x in ("start", "poll", "cancel")]
    return {"fixture_self_test": "passed", "session_keeper_survivor_verified": True,
            "real_browser_opened": False}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, help="Freshly built native ssmcp executable")
    parser.add_argument("--output", type=Path, help="Write a JSON evidence report")
    parser.add_argument("--expect-sha256", help="Require the freshly built binary digest")
    parser.add_argument("--self-test", action="store_true", help="Check PTY/HTTP fixture mechanics only")
    parser.add_argument("--ssh-fixture-self-test", action="store_true",
                        help="Check disposable loopback sshd and encrypted-key authentication only")
    parser.add_argument("--with-ssh-prompts", action="store_true",
                        help="Also exercise real host-key/passphrase prompts and cancellation")
    args = parser.parse_args()
    if sys.version_info < (3, 11):
        parser.error("this fixture requires Python 3.11 or newer")
    if sys.platform != "linux":
        parser.error("this fixture requires Linux /proc, pidfds, and xdg-open")
    if not args.self_test and not args.ssh_fixture_self_test and not args.binary:
        parser.error("--binary is required unless --self-test is selected")
    binary = args.binary.resolve() if args.binary else None
    if binary and (not binary.is_file() or not os.access(binary, os.X_OK)):
        parser.error("the --binary path is not an executable file")
    ssh_name = shutil.which("ssh")
    if not args.self_test and not ssh_name:
        parser.error("a real system OpenSSH client is required")
    ssh = Path(ssh_name).resolve() if ssh_name else None
    report = {"status": "failed", "cases": [],
              "fixture_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest()}
    try:
        with tempfile.TemporaryDirectory(prefix="ssmcp onboarding e2e ") as temporary:
            root = Path(temporary)
            report.update(self_test(root))
            if args.ssh_fixture_self_test:
                with SshServer(root / "server-self-test", ssh):
                    report["encrypted_real_ssh_fixture"] = "passed"
            elif not args.self_test:
                # Freeze the exact executable so a later build cannot change the
                # tested artifact between cases.
                frozen_binary = root / "ssmcp-under-test"
                shutil.copyfile(binary, frozen_binary)
                frozen_binary.chmod(0o500)
                digest = hashlib.sha256(frozen_binary.read_bytes()).hexdigest()
                if args.expect_sha256:
                    assert digest == args.expect_sha256.lower(), "binary digest differs from the declared build"
                report["binary_sha256"] = digest
                report["binary_source"] = str(binary)
                version_result = subprocess.run([str(frozen_binary), "--version"],
                                                capture_output=True, text=True, timeout=5)
                assert version_result.returncode == 0, version_result.stderr
                report["binary_version"] = version_result.stdout.strip()
                version = subprocess.run(
                    [str(ssh), "-V"], capture_output=True, text=True, timeout=5,
                )
                assert version.returncode == 0 and "OpenSSH" in version.stderr + version.stdout
                report["openssh_version"] = (version.stderr + version.stdout).strip()
                # Reserving a bound but non-listening port prevents another process
                # from turning the intended refusal into an unintended real service.
                with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as closed:
                    closed.bind(("127.0.0.1", 0))
                    main_cases(frozen_binary, root, ssh, closed.getsockname()[1], report["cases"])
                if args.with_ssh_prompts:
                    prompt_cases(frozen_binary, root, ssh, report["cases"])
            report["status"] = "passed"
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n",
                                   encoding="utf-8")
        print(json.dumps(report, ensure_ascii=False, indent=2), flush=True)


if __name__ == "__main__":
    if len(sys.argv) > 1 and sys.argv[1] == "--exec-pty-keeper":
        # The keeper, not ssmcp, owns the terminal session. It survives CLI exit
        # until the test releases it, so session-leader SIGHUP cannot hide leaks.
        release_fd = int(sys.argv[2])
        identity_file = Path(sys.argv[3])
        exit_file = Path(sys.argv[4])
        fcntl.ioctl(0, termios.TIOCSCTTY, 0)
        settings = termios.tcgetattr(0)
        settings[3] |= termios.ISIG
        settings[6][termios.VINTR] = b"\x03"
        termios.tcsetattr(0, termios.TCSANOW, settings)
        child = subprocess.Popen(
            sys.argv[5:], stdin=0, stdout=1, stderr=2, close_fds=True,
            process_group=0,
        )
        identity = process_identity(child.pid)
        assert identity is not None, "CLI exited before its identity could be recorded"
        temporary = identity_file.with_suffix(".tmp")
        temporary.write_text(json.dumps(identity))
        temporary.replace(identity_file)
        os.tcsetpgrp(0, child.pid)
        status = child.wait()
        temporary = exit_file.with_suffix(".tmp")
        temporary.write_text(json.dumps({"returncode": status}))
        temporary.replace(exit_file)
        try:
            os.read(release_fd, 1)
        finally:
            os.close(release_fd)
        raise SystemExit(0)
    else:
        main()
