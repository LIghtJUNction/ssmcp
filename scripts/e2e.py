#!/usr/bin/env python3
"""Exercise the actual Rust binaries through an isolated, public-key-only sshd.

Uses only the Python standard library. It does not read existing SSH identities,
change system configuration, or contact any remote host.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import http.client
import json
import os
from pathlib import Path
import pwd
import re
import selectors
import shutil
import signal
import socket
import subprocess
import tempfile
import time


MODERN = "2026-07-28"
LEGACY = "2025-11-25"
PUBLIC_URL = "https://ssmcp.example"
PROTOCOL_META = "io.modelcontextprotocol/protocolVersion"
CAPABILITIES_META = "io.modelcontextprotocol/clientCapabilities"


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def process_running(pid: int) -> bool:
    try:
        # An exited child may briefly remain as a zombie until its owner reaps it.
        return Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0] != "Z"
    except FileNotFoundError:
        return False


def wait_for(predicate, *, timeout=5.0, description="condition"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.03)
    raise AssertionError(f"Timed out waiting for {description}")


class Harness:
    def __init__(self, directory: Path, binary_dir: Path):
        self.directory = directory
        self.binary_dir = binary_dir.resolve()
        self.user = pwd.getpwuid(os.getuid()).pw_name
        self.http_port = free_port()
        self.ssh_port = free_port()
        while self.ssh_port == self.http_port:
            self.ssh_port = free_port()
        self.socket = directory / "control.sock"
        self.processes = []
        self.logs = []
        self.checks = []
        self.request_id = 0
        self.secret = ""
        self.client = None

    def check(self, condition, description):
        if not condition:
            raise AssertionError(description)
        self.checks.append(description)
        print(f"PASS {description}", flush=True)

    def spawn(self, label, command, *, stdout=None):
        log_path = self.directory / f"{label}.log"
        log = log_path.open("w+")
        self.logs.append((log_path, log))
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=stdout if stdout is not None else log,
            stderr=log,
            start_new_session=True,
        )
        self.processes.append(process)
        return process

    @staticmethod
    def stop(process):
        if process.poll() is None:
            process.send_signal(signal.SIGTERM)
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait(timeout=5)

    def cleanup(self):
        for process in reversed(self.processes):
            self.stop(process)
            if process.stdout:
                process.stdout.close()
        for _, log in self.logs:
            log.close()

    def diagnostics(self):
        for path, log in self.logs:
            log.flush()
            text = path.read_text(errors="replace")[-4000:]
            # Successful client stdout is deliberately excluded: it has a secret.
            if self.secret:
                text = text.replace(self.secret, "[redacted]")
            if text:
                print(f"\n{path.name}:\n{text}", flush=True)

    def request(self, method="POST", body=None, *, headers=None, secret=None, path=None):
        request_headers = {
            "Accept": "application/json, text/event-stream",
            "Content-Type": "application/json",
        }
        token = self.secret if secret is None else secret
        if token:
            request_headers["Authorization"] = f"Bearer {token}"
        if headers:
            request_headers.update(headers)
        raw_body = body if isinstance(body, (bytes, str)) else json.dumps(body).encode()
        if body is None:
            raw_body = None
        connection = http.client.HTTPConnection("127.0.0.1", self.http_port, timeout=10)
        try:
            connection.request(method, path or f"/{self.user}", body=raw_body, headers=request_headers)
            response = connection.getresponse()
            payload = response.read()
            response_headers = dict(response.getheaders())
            try:
                result = json.loads(payload) if payload else None
            except json.JSONDecodeError:
                result = payload.decode(errors="replace")
            return response.status, result, response_headers
        finally:
            connection.close()

    def rpc(self, method, params=None, *, modern=False, notification=False, headers=None):
        self.request_id += 1
        payload = {"jsonrpc": "2.0", "method": method, "params": dict(params or {})}
        if not notification:
            payload["id"] = self.request_id
        rpc_headers = {"MCP-Protocol-Version": MODERN if modern else LEGACY}
        if modern:
            payload["params"]["_meta"] = {PROTOCOL_META: MODERN, CAPABILITIES_META: {}}
            rpc_headers["Mcp-Method"] = method
            if method == "tools/call":
                rpc_headers["Mcp-Name"] = payload["params"]["name"]
        if headers:
            rpc_headers.update(headers)
        return self.request(body=payload, headers=rpc_headers)

    def call(self, name, arguments=None, *, modern=True):
        status, result, _ = self.rpc(
            "tools/call", {"name": name, "arguments": arguments or {}}, modern=modern
        )
        if status != 200 or "error" in result or result["result"].get("isError"):
            raise AssertionError(f"Tool {name} failed: HTTP {status}, {result}")
        return result["result"]["structuredContent"]

    def read_until(self, pattern, *, cursor=0, timeout=5.0):
        deadline = time.monotonic() + timeout
        output = ""
        while time.monotonic() < deadline:
            result = self.call("terminal.read", {"cursor": cursor, "max_bytes": 32768, "wait_ms": 300})
            decoded = base64.b64decode(result["data_base64"]).decode(errors="replace")
            if decoded != result["text"]:
                raise AssertionError("PTY text and base64 bytes disagree")
            output += result["text"]
            cursor = result["next_cursor"]
            match = re.search(pattern, output)
            if match:
                return match, cursor, output
        raise AssertionError(f"Expected PTY output did not arrive: {pattern!r}; received {output[-2000:]!r}")

    def prepare(self):
        for executable in ("ssmcp", "ssmcpd"):
            if not (self.binary_dir / executable).is_file():
                raise RuntimeError(f"Missing {self.binary_dir / executable}; run cargo build --locked first")
        sshd = shutil.which("sshd") or "/usr/sbin/sshd"
        if not Path(sshd).is_file():
            raise RuntimeError("OpenSSH server is required (install openssh-server)")
        for key in ("host", "client", "wrong"):
            subprocess.run(
                ["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-f", str(self.directory / key)],
                check=True,
                stdout=subprocess.DEVNULL,
            )
        (self.directory / "authorized_keys").write_text((self.directory / "client.pub").read_text())
        (self.directory / "known_hosts").write_text(
            "ssmcp-e2e " + (self.directory / "host.pub").read_text()
        )
        for name, key in (("ssh_config", "client"), ("wrong_config", "wrong")):
            (self.directory / name).write_text(
                f"Host ssmcp-e2e\n"
                f"    HostName 127.0.0.1\n"
                f"    HostKeyAlias ssmcp-e2e\n"
                f"    Port {self.ssh_port}\n"
                f"    IdentityFile {self.directory / key}\n"
                f"    IdentitiesOnly yes\n"
                f"    UserKnownHostsFile {self.directory / 'known_hosts'}\n"
                f"    GlobalKnownHostsFile /dev/null\n"
                f"    StrictHostKeyChecking yes\n"
                f"    BatchMode yes\n"
                f"    LogLevel ERROR\n"
            )
        config = self.directory / "sshd_config"
        config.write_text(
            f"Port {self.ssh_port}\n"
            f"ListenAddress 127.0.0.1\n"
            f"HostKey {self.directory / 'host'}\n"
            f"PidFile {self.directory / 'sshd.pid'}\n"
            f"AuthorizedKeysFile {self.directory / 'authorized_keys'}\n"
            f"StrictModes no\n"
            f"UsePAM no\n"
            f"AllowUsers {self.user}\n"
            f"PermitRootLogin prohibit-password\n"
            f"AuthenticationMethods publickey\n"
            f"PubkeyAuthentication yes\n"
            f"PasswordAuthentication no\n"
            f"KbdInteractiveAuthentication no\n"
            f"DisableForwarding yes\n"
            f"PermitTTY no\n"
            f"PermitUserRC no\n"
            f"ClientAliveInterval 1\n"
            f"ClientAliveCountMax 2\n"
            f"ForceCommand {self.binary_dir / 'ssmcpd'} attach --socket {self.socket}\n"
        )
        effective = subprocess.check_output(
            [sshd, "-T", "-f", str(config), "-C", f"user={self.user},host=localhost,addr=127.0.0.1"],
            text=True,
        )
        required = (
            "authenticationmethods publickey", "passwordauthentication no",
            "kbdinteractiveauthentication no", "disableforwarding yes", "permittty no",
        )
        self.check(all(item in effective.splitlines() for item in required), "effective sshd config permits public keys only")
        daemon = self.spawn("daemon", [
            str(self.binary_dir / "ssmcpd"), "serve", "--listen", f"127.0.0.1:{self.http_port}",
            "--public-url", PUBLIC_URL, "--socket", str(self.socket),
        ])

        def healthy():
            if daemon.poll() is not None:
                raise AssertionError("ssmcpd exited before opening its listeners; see daemon.log")
            try:
                return self.request("GET", path="/healthz", secret="")[0] == 200
            except OSError:
                return False

        wait_for(healthy, description="daemon health")
        self.spawn("sshd", [sshd, "-D", "-e", "-f", str(config)])

        def ssh_ready():
            try:
                with socket.create_connection(("127.0.0.1", self.ssh_port), timeout=0.2):
                    return True
            except OSError:
                return False

        wait_for(ssh_ready, description="isolated sshd")

    def start_client(self, label="client"):
        process = self.spawn(label, [
            str(self.binary_dir / "ssmcp"), "-F", str(self.directory / "ssh_config"),
            "--shell", "/bin/sh", f"{self.user}@ssmcp-e2e",
        ], stdout=subprocess.PIPE)
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            if not selector.select(timeout=10):
                raise AssertionError("ssmcp did not return connection credentials")
        line = process.stdout.readline()
        if not line:
            raise AssertionError("ssmcp exited before returning connection credentials")
        return process, json.loads(line)

    def run(self):
        self.prepare()
        bad_key = subprocess.run([
            "ssh", "-F", str(self.directory / "wrong_config"), "-T", f"{self.user}@ssmcp-e2e"
        ], capture_output=True, timeout=5)
        self.check(bad_key.returncode == 255, "unauthorized public key is rejected by real sshd")
        password = subprocess.run([
            "ssh", "-F", str(self.directory / "ssh_config"), "-T",
            "-oPreferredAuthentications=password,keyboard-interactive", "-oPubkeyAuthentication=no",
            f"{self.user}@ssmcp-e2e",
        ], capture_output=True, timeout=5)
        self.check(password.returncode == 255, "password and keyboard-interactive cannot authenticate")
        self.client, credentials = self.start_client()
        self.secret = credentials["secret"]
        self.check(credentials["url"] == f"{PUBLIC_URL}/{self.user}", "endpoint username comes from SSH account")
        self.check(bool(re.fullmatch(r"[0-9a-f]{64}", self.secret)), "secret contains 256 random bits encoded as hex")
        self.check(self.request(body={}, secret="")[0] == 401, "missing Bearer is rejected")
        self.check(self.request(body={}, secret="0" * 64)[0] == 401, "incorrect Bearer is rejected")
        self.check(self.request(body={}, path="/different-user")[0] == 401, "secret cannot select another user")
        self.check(self.request(body={}, headers={"Origin": "https://attacker.example"})[0] == 403, "foreign Origin is rejected")

        for version in ("2025-03-26", "2025-06-18", LEGACY):
            status, result, _ = self.rpc("initialize", {
                "protocolVersion": version, "capabilities": {},
                "clientInfo": {"name": "ssmcp-e2e", "version": "1"},
            })
            self.check(status == 200 and result["result"]["protocolVersion"] == version,
                       f"legacy initialize negotiates {version}")
        status, result, _ = self.rpc("notifications/initialized", notification=True)
        self.check(status == 202 and result is None, "initialized notification returns empty 202")
        status, result, headers = self.rpc("tools/list")
        expected_tools = {"terminal.read", "terminal.write", "terminal.resize", "terminal.status"}
        self.check(status == 200 and {tool["name"] for tool in result["result"]["tools"]} == expected_tools,
                   "legacy tools/list exposes exactly four terminal tools")
        self.check(any(key.lower() == "cache-control" and "no-store" in value for key, value in headers.items()),
                   "MCP responses cannot be stored by caches")
        self.check(self.rpc("ping", headers={"Origin": PUBLIC_URL})[0] == 200, "configured Origin is accepted")
        self.check(self.request("GET")[0] == 405 and self.request("DELETE")[0] == 405,
                   "unsupported GET and DELETE return 405")
        self.check(self.request(body=b"{")[0] == 400, "malformed JSON is rejected")
        self.check(self.request(body=[])[0] == 400, "JSON-RPC batches are rejected")
        self.check(self.rpc("ping", headers={"MCP-Protocol-Version": "1900-01-01"})[0] == 400,
                   "unsupported protocol version is rejected")
        self.check(self.request(body=b"x" * (256 * 1024 + 1))[0] == 413, "oversize HTTP body is rejected")

        status, result, _ = self.rpc("server/discover", modern=True)
        self.check(status == 200 and result["result"]["resultType"] == "complete"
                   and MODERN in result["result"]["supportedVersions"], "modern server/discover completes")
        self.check(self.rpc("tools/list", modern=True)[0] == 200, "modern tools/list succeeds")
        self.check(self.rpc("tools/list", modern=True, headers={"Mcp-Method": "ping"})[0] == 400,
                   "modern method header must match body")
        self.check(self.rpc("tools/list", modern=True, headers={"MCP-Protocol-Version": LEGACY})[0] == 400,
                   "modern version header must match metadata")
        status, result, _ = self.rpc("tools/call", {"name": "terminal.status", "arguments": {}},
                                     modern=True, headers={"Mcp-Name": "terminal.write"})
        self.check(status == 400, "modern tool name header must match body")
        encoded = "=?base64?" + base64.b64encode(b"terminal.status").decode() + "?="
        self.check(self.rpc("tools/call", {"name": "terminal.status", "arguments": {}},
                            modern=True, headers={"Mcp-Name": encoded})[0] == 200,
                   "modern encoded tool name is decoded")
        self.check(self.rpc("unknown/method", modern=True)[0] == 404, "unknown modern method returns 404")
        status, result, _ = self.rpc("tools/call", {
            "name": "terminal.write",
            "arguments": {"data": "printf '%s%s\\n' 'SSMCP_' 'NOTIFICATION_EXECUTED'\n"},
        }, modern=True, notification=True)
        self.check(status == 202 and result is None, "tool notification is acknowledged without a response")

        duplicate = self.spawn("duplicate", [
            str(self.binary_dir / "ssmcp"), "-F", str(self.directory / "ssh_config"), f"{self.user}@ssmcp-e2e",
        ], stdout=subprocess.PIPE)
        duplicate.wait(timeout=5)
        self.check(duplicate.returncode != 0 and not duplicate.stdout.read(), "duplicate user connection is refused")
        self.check(self.call("terminal.status")["running"], "original terminal survives duplicate connection")
        self.call("terminal.resize", {"rows": 37, "cols": 91})
        self.call("terminal.write", {"data": "stty size; tty; printf '%s%s:%s\\n' 'SSMCP_' 'SHELL' \"$$\"\n"})
        match, cursor, output = self.read_until(r"(?:\r?\n)SSMCP_SHELL:(\d+)\r?\n")
        shell_pid = int(match.group(1))
        self.check("SSMCP_NOTIFICATION_EXECUTED" not in output, "tool notifications never execute terminal operations")
        self.check("37 91" in output and "/dev/pts/" in output, "resize and tty commands run in a real local PTY")
        self.call("terminal.write", {"data": "printf '%s%s\\n' '中文-' '终端'; cd /; pwd\n"})
        _, cursor, output = self.read_until(r"(?:\r?\n)中文-终端\r?\n/\r?\n", cursor=cursor)
        self.check(True, "UTF-8 output and persistent working directory round-trip")
        self.call("terminal.write", {"data": "sleep 30\n"})
        time.sleep(0.15)
        self.call("terminal.write", {"data": "\u0003"})
        self.call("terminal.write", {"data": "printf '%s%s\\n' 'SSMCP_' 'INTERRUPTED'\n"})
        _, cursor, _ = self.read_until(r"(?:\r?\n)SSMCP_INTERRUPTED\r?\n", cursor=cursor, timeout=3)
        self.check(True, "Ctrl-C interrupts the foreground process")

        # Drain the prompt, then demonstrate that a waiting read does not block a write.
        result = self.call("terminal.read", {"cursor": cursor, "wait_ms": 100})
        cursor = result["next_cursor"]
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
            waiting = pool.submit(self.call, "terminal.read", {"cursor": cursor, "wait_ms": 5000})
            time.sleep(0.1)
            start = time.monotonic()
            self.call("terminal.write", {"data": "printf '%s%s\\n' 'SSMCP_' 'CONCURRENT'\n"})
            result = waiting.result(timeout=3)
            self.check(time.monotonic() - start < 3 and result["next_cursor"] > cursor,
                       "waiting reads allow concurrent writes")
        _, cursor, _ = self.read_until(r"(?:\r?\n)SSMCP_CONCURRENT\r?\n", cursor=cursor)

        self.call("terminal.write", {"data": "sleep 300 & printf '%s%s:%s\\n' 'SSMCP_' 'CHILD' \"$!\"\n"})
        match, cursor, _ = self.read_until(r"(?:\r?\n)SSMCP_CHILD:(\d+)\r?\n", cursor=cursor)
        child_pid = int(match.group(1))
        self.call("terminal.write", {"data": "sleep 300\n"})
        time.sleep(0.1)
        self.stop(self.client)
        wait_for(lambda: self.rpc("ping")[0] == 401, description="revocation after client disconnect")
        self.check(True, "disconnect immediately revokes the old Bearer")
        wait_for(lambda: not process_running(shell_pid), description="PTY shell exit")
        wait_for(lambda: not process_running(child_pid), description="background terminal job exit")
        self.check(True, "disconnect cleans up shell and terminal jobs")
        old_secret = self.secret
        self.client, credentials = self.start_client("reconnect")
        self.secret = credentials["secret"]
        self.check(self.secret != old_secret and self.request(body={}, secret=old_secret)[0] == 401,
                   "reconnect rotates credentials and rejects the previous secret")
        self.check(self.call("terminal.status")["running"], "reconnect creates a usable new terminal")
        self.stop(self.client)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--report", type=Path, help="write a JSON report without credentials")
    args = parser.parse_args()
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-e2e-") as directory:
        harness = Harness(Path(directory), args.bin_dir)
        try:
            harness.run()
        except Exception:
            harness.diagnostics()
            raise
        finally:
            harness.cleanup()
        report = {
            "status": "passed", "checks": harness.checks,
            "duration_seconds": round(time.monotonic() - started, 3),
            "transport": "real OpenSSH public-key + HTTP + Linux PTY",
        }
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(f"\nPassed {len(harness.checks)} end-to-end checks in {report['duration_seconds']}s")


if __name__ == "__main__":
    main()
