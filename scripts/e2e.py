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
import hashlib
from html.parser import HTMLParser
import json
import os
from pathlib import Path
import pwd
import re
import selectors
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time
from urllib.parse import urlencode, urlsplit, parse_qs


MODERN = "2026-07-28"
LEGACY = "2025-11-25"
PUBLIC_URL = "https://ssmcp.example"
PROTOCOL_META = "io.modelcontextprotocol/protocolVersion"
CAPABILITIES_META = "io.modelcontextprotocol/clientCapabilities"

SHELL_TOOLS = frozenset({
    "terminal.read", "terminal.write", "terminal.resize", "terminal.status",
})
BUILTIN_TOOLS = frozenset({
    "ssmcp.fs.list", "ssmcp.fs.stat", "ssmcp.fs.read", "ssmcp.fs.write", "ssmcp.fs.mkdir",
    "ssmcp.fs.read_text", "ssmcp.fs.write_text", "ssmcp.fs.edit_text", "ssmcp.fs.events",
    "ssmcp.context.roots", "ssmcp.skills.list", "ssmcp.skills.read", "ssmcp.skills.view",
    "ssmcp.agents.list", "ssmcp.agents.read", "ssmcp.agents.resolve",
    "ssmcp.net.listen", "ssmcp.net.accept", "ssmcp.net.connect", "ssmcp.net.read",
    "ssmcp.net.write", "ssmcp.net.shutdown", "ssmcp.net.close", "ssmcp.net.list",
    "ssmcp.rsync.start", "ssmcp.rsync.read", "ssmcp.rsync.write",
    "ssmcp.rsync.status", "ssmcp.rsync.close",
    "ssmcp.client.reload",
    "ssmcp.output.read", "ssmcp.output.stat", "ssmcp.output.cleanup",
    "ssmcp.exec.start", "ssmcp.exec.list", "ssmcp.exec.status", "ssmcp.exec.wait",
    "ssmcp.exec.read", "ssmcp.exec.cancel", "ssmcp.exec.forget", "ssmcp.exec.events",
    "ssmcp.harnesses.list", "ssmcp.audit.events",
})


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
        self.sensitive = []
        self.oauth_client = None
        self.oauth_grant = None
        self.client = None
        self.client_id = ""

    def check(self, condition, description):
        if not condition:
            raise AssertionError(description)
        self.checks.append(description)
        print(f"PASS {description}", flush=True)

    def spawn(self, label, command, *, stdout=None, cwd=None, env=None):
        log_path = self.directory / f"{label}.log"
        log = log_path.open("w+")
        self.logs.append((log_path, log))
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=stdout if stdout is not None else log,
            stderr=log,
            start_new_session=True,
            cwd=cwd,
            env=env,
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
            for secret in [self.secret, *self.sensitive]:
                if secret:
                    text = text.replace(secret, "[redacted]")
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

    def rpc(self, method, params=None, *, modern=False, notification=False, headers=None, secret=None):
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
        return self.request(body=payload, headers=rpc_headers, secret=secret)

    def public_call(self, name, arguments=None, *, modern=True, secret=None):
        status, result, _ = self.rpc(
            "tools/call", {"name": name, "arguments": arguments or {}}, modern=modern, secret=secret
        )
        if status != 200 or "error" in result or result["result"].get("isError"):
            raise AssertionError(f"Tool {name} failed: HTTP {status}, {result}")
        return result["result"]["structuredContent"]

    def call(self, name, arguments=None, *, modern=True, client_id=None, secret=None):
        """Call a native capability through the public JavaScript interface."""
        code = "return await call(%s, %s, %s);" % (
            json.dumps(client_id or self.client_id), json.dumps(name), json.dumps(arguments or {}),
        )
        result = self.public_call("code_mode", {"code": code}, modern=modern, secret=secret)["result"]
        if result.get("isError"):
            raise AssertionError(f"Native tool {name} failed: {result}")
        return result["structuredContent"]

    def workspace_roundtrip(self, directory, *, client_id=None, label="workspace"):
        """Exercise universal file helpers across actual SSH in a temporary launch root."""
        payload = bytes(range(256)) * 17 + b"\x00ssmcp SSH roundtrip\xff"
        relative = "workspace-check/nested/binary.dat"
        code = """
            const id = %s;
            await fs.mkdir(id, {path:"workspace-check/nested", parents:true});
            await fs.write(id, {path:%s, data_base64:%s});
            const read = await fs.read(id, {path:%s});
            const stat = await fs.stat(id, {path:%s});
            const root = await fs.stat(id, {});
            return {read, stat, root};
        """ % (
            json.dumps(client_id or self.client_id), json.dumps(relative),
            json.dumps(base64.b64encode(payload).decode()), json.dumps(relative),
            json.dumps(relative),
        )
        result = self.public_call("code_mode", {"code": code})["result"]
        self.check(result["root"]["root"] == str(directory.resolve())
                   and result["root"]["kind"] == "directory",
                   f"{label}: fs.stat identifies the SSH client's exact shared root")
        self.check(base64.b64decode(result["read"]["data_base64"], validate=True) == payload
                   and result["read"]["eof"] and result["stat"]["kind"] == "file"
                   and result["stat"]["size"] == len(payload)
                   and (directory / relative).read_bytes() == payload,
                   f"{label}: fs.mkdir/write/read/stat preserve binary bytes through real SSH")

        skill = "---\nname: ssh-skill\ndescription: Inspect files through SSH.\n---\nSKILL_BODY_ONLY_ON_READ\n"
        text = "\ufeff原生文件🙂\r\nbefore\r\n"
        code = r"""
            const id = %s;
            const initial = await fs.writeText(id, {
                path:"workspace-check/text.txt", text:%s
            });
            const read = await fs.readText(id, {path:"workspace-check/text.txt"});
            const edited = await fs.editText(id, {
                path:"workspace-check/text.txt", old_text:"before", new_text:"after",
                expected_sha256:read.sha256
            });
            let stale = false;
            try {
                await fs.editText(id, {
                    path:"workspace-check/text.txt", old_text:"after", new_text:"stale",
                    expected_sha256:read.sha256
                });
            } catch (_) { stale = true; }
            const current = await fs.readText(id, {path:"workspace-check/text.txt"});
            await fs.writeText(id, {path:"skills/ssh-skill/SKILL.md",text:%s,create_parents:true});
            await fs.writeText(id, {path:"workspace-check/AGENTS.md",text:"Parent rule.\n"});
            await fs.writeText(id, {path:"workspace-check/nested/agents.md",text:"Nested rule.\n"});
            const roots = await context.roots(id);
            const index = await skills.list(id);
            const view = await skills.view(id);
            const full = await skills.read(id, {path:"skills/ssh-skill/SKILL.md"});
            const rules = await agents.resolve(id, {path:"workspace-check/nested/new.rs"});
            const instruction = await agents.read(id, {path:"workspace-check/AGENTS.md"});
            const skillResource = await resources.read("ssmcp://clients/"+id+"/skills?root=workspace");
            const ruleResource = await resources.read("ssmcp://clients/"+id+"/agents?path=workspace-check%%2Fnested%%2Fnew.rs");
            return {initial,read,edited,current,stale,roots,index,view,full,rules,instruction,
                    skillResource:JSON.parse(skillResource.contents[0].text),
                    ruleResource:JSON.parse(ruleResource.contents[0].text)};
        """ % (
            json.dumps(client_id or self.client_id), json.dumps(text), json.dumps(skill),
        )
        result = self.public_call("code_mode", {"code": code})["result"]
        self.check(result["read"]["text"] == text
                   and result["read"]["sha256"] == hashlib.sha256(text.encode()).hexdigest(),
                   f"{label}: native UTF-8 read/write preserves BOM, Unicode and CRLF")
        self.check(result["edited"]["replacements"] == 1 and result["stale"]
                   and result["current"]["text"] == text.replace("before", "after")
                   and (directory / "workspace-check/text.txt").read_bytes()
                       == text.replace("before", "after").encode(),
                   f"{label}: native text edit enforces SHA-256 preconditions through SSH")
        self.check(result["roots"]["roots"][0]["id"] == "workspace"
                   and result["roots"]["roots"][0]["path"] == str(directory.resolve()),
                   f"{label}: context roots identify the actual SSH client directory")
        summaries = result["index"]["skills"]
        self.check(len(summaries) == 1 and summaries[0]["name"] == "ssh-skill"
                   and "SKILL_BODY_ONLY_ON_READ" not in json.dumps(result["index"])
                   and "Inspect files through SSH." in result["view"]["markdown"],
                   f"{label}: skill discovery generates summaries without loading body text into the view")
        self.check(result["full"]["content"] == skill
                   and result["full"]["root"]["id"] == "workspace",
                   f"{label}: complete skill contents are read on demand with provenance")
        self.check([row["path"] for row in result["rules"]["files"]]
                   == ["workspace-check/AGENTS.md", "workspace-check/nested/agents.md"]
                   and [row["content"] for row in result["rules"]["files"]]
                   == ["Parent rule.\n", "Nested rule.\n"]
                   and result["instruction"]["content"] == "Parent rule.\n",
                   f"{label}: AGENTS resolution keeps parent-to-target order and exact text")
        self.check(result["skillResource"]["skills"][0]["name"] == "ssh-skill"
                   and result["ruleResource"]["files"] == result["rules"]["files"],
                   f"{label}: dynamic skill and instruction resources route across actual SSH")

    def read_until(self, pattern, *, cursor=0, timeout=5.0, client_id=None):
        deadline = time.monotonic() + timeout
        output = ""
        while time.monotonic() < deadline:
            result = self.call("terminal.read", {"cursor": cursor, "max_bytes": 32768, "wait_ms": 300},
                               client_id=client_id)
            decoded = base64.b64decode(result["data_base64"]).decode(errors="replace")
            if decoded != result["text"]:
                raise AssertionError("PTY text and base64 bytes disagree")
            output += result["text"]
            cursor = result["next_cursor"]
            # Modern readline emits bracketed-paste toggles and an extra CR
            # before command output. Match visible text, while the byte-for-byte
            # text/base64 assertion above still checks the actual transport.
            visible = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", output).replace("\n\r", "\n")
            match = re.search(pattern, visible)
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
                f"    User {self.user}\n"
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
        self.check(all(item in effective.lower().splitlines() for item in required), "effective sshd config permits public keys only")
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

    def client_environment(self, label, *, harness=None, fixture=None, fixture_args=(), harnesses=()):
        """Expose only shell utilities and the explicitly selected local fixture.

        The production client discovers coding harnesses from PATH. Never inherit
        installed harnesses here: the tests must not start a real model account.
        """
        directory = self.directory / f"{label}-bin"
        directory.mkdir(mode=0o700)
        for name in ("ssh", "stty", "tty", "sleep", "head", "tr"):
            executable = shutil.which(name)
            if not executable:
                raise RuntimeError(f"test requires system {name}")
            (directory / name).symlink_to(Path(executable).absolute())
        # Fixtures have env-based shebangs; retain the running interpreter even
        # when the rest of the host PATH is deliberately unavailable.
        (directory / "python3").symlink_to(Path(sys.executable).absolute())
        installed = list(harnesses)
        if harness is not None:
            installed.append((harness, fixture, fixture_args))
        elif fixture is not None or fixture_args:
            raise ValueError("fixture arguments require an explicit harness")
        names = {"codex": "codex", "cc": "claude", "pi": "pi", "opencode": "opencode"}
        for kind, program, arguments in installed:
            if kind not in names or program is None:
                raise ValueError("a known harness and local fixture must be supplied together")
            wrapper = directory / names[kind]
            if wrapper.exists():
                raise ValueError(f"duplicate harness fixture: {kind}")
            command = shlex.join([sys.executable, str(Path(program).resolve())])
            extra = shlex.join([str(argument) for argument in arguments])
            wrapper.write_text(
                f'#!/bin/sh\nif [ "$1" = "--version" ]; then printf "%s\\n" '
                f'"{names[kind]} fixture 2.3.0"; exit 0; fi\n'
                f'exec {command} "$@" {extra}\n'
            )
            wrapper.chmod(0o700)
        inherited = {key: value for key, value in os.environ.items()
                     if key not in ("SSMCP_PI_RPC_SOCKET", "SSMCP_OPENCODE_URL", "OPENCODE_PASSWORD", "OPENCODE_SERVER_PASSWORD")}
        return {
            **inherited,
            "PATH": str(directory.resolve()),
            "ENV": "/dev/null",
            "BASH_ENV": "/dev/null",
        }

    def start_client(self, label="client", *, name="workstation", cwd=None, harness=None,
                     fixture=None, fixture_args=(), destination=None, harnesses=(), extra_env=None):
        command = [
            str(self.binary_dir / "ssmcp"), "-F", str(self.directory / "ssh_config"),
            "--name", name, "--shell", "/bin/sh",
            destination or f"{self.user}@ssmcp-e2e",
        ]
        environment = self.client_environment(
            label, harness=harness, fixture=fixture, fixture_args=fixture_args, harnesses=harnesses
        )
        if extra_env:
            environment.update(extra_env)
        process = self.spawn(label, command, stdout=subprocess.PIPE, cwd=cwd, env=environment)
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            if not selector.select(timeout=10):
                raise AssertionError("ssmcp did not return connection credentials")
        line = process.stdout.readline()
        if not line:
            raise AssertionError("ssmcp exited before returning connection credentials")
        return process, json.loads(line)

    def oauth_request(self, path, method="GET", data=None, *, cookie=None, headers=None, form=False):
        extra = {"Accept": "application/json"}
        if form:
            extra["Content-Type"] = "application/x-www-form-urlencoded"
            data = urlencode(data or {})
        if cookie:
            extra["Cookie"] = cookie
        if headers:
            extra.update(headers)
        return self.request(method, data, path=path, secret="", headers=extra)

    def oauth_login(self, credentials, *, attacks=False):
        resource = credentials["url"]
        login_code = credentials["login_code"]
        self.sensitive.append(login_code)
        self.check("secret" not in credentials, "SSH returns an OAuth login code, not a reusable bearer secret")
        self.check(bool(re.fullmatch(r"[0-9a-f]{64}", login_code)), "login code contains 256 random bits")
        self.check(0 < credentials["login_expires_in"] <= 1800, "login code reports its remaining thirty-minute lifetime")
        self.check(self.request(body={}, secret=login_code)[0] == 401, "one-time login code cannot authenticate an MCP request")
        status, _, challenge = self.request(body={}, secret="")
        metadata_url = f"{PUBLIC_URL}/.well-known/oauth-protected-resource/{self.user}"
        self.check(status == 401 and metadata_url in challenge.get("www-authenticate", ""), "MCP 401 advertises protected-resource metadata")
        status, metadata, _ = self.oauth_request(urlsplit(metadata_url).path)
        self.check(status == 200 and metadata["resource"] == resource and metadata["authorization_servers"] == [PUBLIC_URL], "resource metadata binds the exact user endpoint and OAuth issuer")
        status, server, _ = self.oauth_request("/.well-known/oauth-authorization-server")
        self.check(status == 200 and server["issuer"] == PUBLIC_URL and "S256" in server["code_challenge_methods_supported"], "OAuth discovery advertises authorization-code PKCE S256")
        if self.oauth_client is None:
            status, registration, _ = self.oauth_request(urlsplit(server["registration_endpoint"]).path, "POST", {
                "client_name": "ssmcp end-to-end", "redirect_uris": ["http://127.0.0.1:37401/callback"],
                "token_endpoint_auth_method": "none", "grant_types": ["authorization_code", "refresh_token"],
                "response_types": ["code"], "application_type": "native",
            })
            self.check(status == 201 and registration.get("client_id"), "public MCP client registers through OAuth DCR")
            self.oauth_client = registration
        client_id = self.oauth_client["client_id"]
        redirect_uri = self.oauth_client["redirect_uris"][0]
        verifier = base64.urlsafe_b64encode(os.urandom(32)).rstrip(b"=").decode()
        challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
        state = base64.urlsafe_b64encode(os.urandom(18)).decode()
        authorize = urlsplit(server["authorization_endpoint"]).path
        token_path = urlsplit(server["token_endpoint"]).path
        params = {"client_id": client_id, "redirect_uri": redirect_uri, "response_type": "code", "scope": "mcp", "resource": resource,
                  "state": state, "code_challenge": challenge, "code_challenge_method": "S256"}
        if attacks:
            bad = dict(params, redirect_uri="https://attacker.example/callback")
            status, _, headers = self.oauth_request(authorize + "?" + urlencode(bad))
            self.check(status == 400 and "location" not in headers, "OAuth never redirects to an unregistered callback")
            bad = dict(params, code_challenge_method="plain")
            status, result, headers = self.oauth_request(authorize + "?" + urlencode(bad))
            returned = parse_qs(urlsplit(headers.get("location", "")).query)
            self.check(status >= 400 or bool(returned.get("error")), "OAuth rejects a PKCE downgrade to plain")
        status, html, headers = self.oauth_request(authorize + "?" + urlencode(params), headers={"Accept": "text/html"})
        self.check(status == 200 and isinstance(html, str), "OAuth presents the login page")
        class Form(HTMLParser):
            def __init__(self):
                super().__init__()
                self.fields = {}
                self.visible = []
            def handle_starttag(self, tag, attrs):
                attrs = dict(attrs)
                if tag == "input" and attrs.get("name"):
                    if attrs.get("type") == "hidden":
                        self.fields[attrs["name"]] = attrs.get("value", "")
                    else:
                        self.visible.append(attrs["name"])
        form = Form()
        form.feed(html)
        self.check(form.visible == ["login_code"] and "transaction_id" in form.fields, "login page has one visible code field and a CSRF-bound transaction")
        cookie_header = headers.get("set-cookie", "")
        self.check(all(flag in cookie_header.lower() for flag in ("secure", "httponly", "samesite=lax")), "login cookie is Secure, HttpOnly and SameSite Lax")
        self.check("no-store" in headers.get("cache-control", "") and headers.get("referrer-policy") == "same-origin", "login page permits same-origin forms without cross-origin referrer leakage")
        cookie = cookie_header.split(";", 1)[0]
        payload = {**form.fields, "login_code": login_code}
        origin = {"Origin": PUBLIC_URL, "Accept": "text/html"}
        if attacks:
            status, _, headers = self.oauth_request(authorize, "POST", payload, headers=origin, form=True)
            self.check(status in (400, 403) and "location" not in headers, "login rejects a missing browser binding cookie")
            status, _, headers = self.oauth_request(authorize, "POST", payload, cookie=cookie, headers={**origin, "Origin": "https://attacker.example"}, form=True)
            self.check(status in (400, 403) and "location" not in headers, "login rejects a cross-origin form submission")
            status, _, headers = self.oauth_request(authorize, "POST", {**payload, "login_code": "0" * 64}, cookie=cookie, headers=origin, form=True)
            self.check(status != 303 and "location" not in headers, "an invalid login code cannot authorize the MCP client")
        status, _, headers = self.oauth_request(authorize, "POST", payload, cookie=cookie, headers=origin, form=True)
        self.check(status == 303, "one-time login code authorizes the OAuth client")
        callback = urlsplit(headers["location"])
        query = parse_qs(callback.query)
        self.check(callback._replace(query="").geturl() == redirect_uri and query.get("state") == [state] and query.get("iss") == [PUBLIC_URL], "OAuth callback preserves state and identifies its issuer")
        code = query["code"][0]
        self.sensitive.append(code)
        exchange = {"grant_type": "authorization_code", "client_id": client_id, "code": code,
                    "code_verifier": verifier, "redirect_uri": redirect_uri, "resource": resource}
        if attacks:
            status, result, _ = self.oauth_request(token_path, "POST", {**exchange, "code_verifier": "x" * 43}, form=True)
            self.check(status == 400 and result.get("error") == "invalid_grant", "authorization code rejects an incorrect PKCE verifier")
            status, result, _ = self.oauth_request(token_path, "POST", {**exchange, "resource": PUBLIC_URL + "/different-user"}, form=True)
            self.check(status == 400 and result.get("error"), "authorization code cannot be exchanged for another resource")
        status, tokens, token_headers = self.oauth_request(token_path, "POST", exchange, form=True)
        self.check(status == 200 and tokens.get("token_type", "").lower() == "bearer", "OAuth code exchange returns an access token")
        self.check(tokens.get("expires_in") == 28800 and tokens.get("refresh_token"), "OAuth access expires in eight hours and supplies a refresh token")
        self.check("no-store" in token_headers.get("cache-control", ""), "OAuth token responses cannot be cached")
        self.secret = tokens["access_token"]
        self.sensitive += [tokens["access_token"], tokens["refresh_token"]]
        self.oauth_grant = {"tokens": tokens, "exchange": exchange, "token_path": token_path, "authorize": authorize, "authorize_params": params}
        return self.oauth_grant

    def oauth_refresh(self, refresh_token=None, resource=None):
        grant = self.oauth_grant
        return self.oauth_request(grant["token_path"], "POST", {
            "grant_type": "refresh_token", "client_id": self.oauth_client["client_id"],
            "refresh_token": refresh_token or grant["tokens"]["refresh_token"],
            "resource": resource or grant["exchange"]["resource"],
        }, form=True)

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
        self.client, credentials = self.start_client(destination="ssmcp-e2e")
        self.check(credentials["url"] == f"{PUBLIC_URL}/{self.user}",
                   "bare SSH alias reuses configured User, HostName, Port, IdentityFile and host trust")
        self.oauth_login(credentials, attacks=True)
        self.client_id = credentials["client_id"]
        self.check(credentials["url"] == f"{PUBLIC_URL}/{self.user}", "endpoint username comes from SSH account")
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
        self.check(status == 200 and [tool["name"] for tool in result["result"]["tools"]] == ["list", "code_mode"],
                   "legacy tools/list exposes exactly list and code_mode")
        inventory = self.public_call("list")
        self.check(len(inventory["clients"]) == 1
                   and inventory["clients"][0]["id"] == self.client_id
                   and inventory["clients"][0]["name"] == "workstation"
                   and inventory["clients"][0]["cwd"] == str(Path.cwd())
                   and inventory["clients"][0]["type"] == "shell"
                   and isinstance(inventory["code_mode"], dict),
                   "list returns client identity, display name, launch cwd, backend type and code API")
        capabilities = self.public_call("list", {"client_id": self.client_id})
        expected_tools = SHELL_TOOLS | BUILTIN_TOOLS
        self.check({tool["name"] for tool in capabilities["tools"]} == expected_tools
                   and all(isinstance(tool["inputSchema"], dict) for tool in capabilities["tools"]),
                   "list discovers exact native capability names and schemas for code_mode")
        self.check(any(key.lower() == "cache-control" and "no-store" in value for key, value in headers.items()),
                   "MCP responses cannot be stored by caches")
        self.check(self.rpc("ping", headers={"Origin": PUBLIC_URL})[0] == 200, "configured Origin is accepted")
        self.check(self.request("GET")[0] == 405 and self.request("DELETE")[0] == 405,
                   "unsupported GET and DELETE return 405")
        self.check(self.request(body=b"{")[0] == 400, "malformed JSON is rejected")
        self.check(self.request(body=[])[0] == 400, "JSON-RPC batches are rejected")
        self.check(self.rpc("ping", headers={"MCP-Protocol-Version": "1900-01-01"})[0] == 400,
                   "unsupported protocol version is rejected")
        self.check(self.request(body=b"x" * (4 * 1024 * 1024 + 1))[0] == 413, "oversize HTTP body is rejected")

        status, result, _ = self.rpc("server/discover", modern=True)
        self.check(status == 200 and result["result"]["resultType"] == "complete"
                   and MODERN in result["result"]["supportedVersions"], "modern server/discover completes")
        status, result, _ = self.rpc("tools/list", modern=True)
        self.check(status == 200 and [tool["name"] for tool in result["result"]["tools"]] == ["list", "code_mode"],
                   "modern tools/list exposes the same two tools")
        self.check(self.rpc("tools/list", modern=True, headers={"Mcp-Method": "ping"})[0] == 400,
                   "modern method header must match body")
        self.check(self.rpc("tools/list", modern=True, headers={"MCP-Protocol-Version": LEGACY})[0] == 400,
                   "modern version header must match metadata")
        status, result, _ = self.rpc("tools/call", {"name": "list", "arguments": {}},
                                     modern=True, headers={"Mcp-Name": "code_mode"})
        self.check(status == 400, "modern tool name header must match body")
        encoded = "=?base64?" + base64.b64encode(b"list").decode() + "?="
        self.check(self.rpc("tools/call", {"name": "list", "arguments": {}},
                            modern=True, headers={"Mcp-Name": encoded})[0] == 200,
                   "modern encoded tool name is decoded")
        self.check(self.rpc("unknown/method", modern=True)[0] == 404, "unknown modern method returns 404")
        status, result, _ = self.rpc("tools/call", {
            "name": "code_mode",
            "arguments": {"code": "return await call(%s, 'terminal.write', %s);" % (
                json.dumps(self.client_id),
                json.dumps({"data": "printf '%s%s\\n' 'SSMCP_' 'NOTIFICATION_EXECUTED'\n"}),
            )},
        }, modern=True, notification=True)
        self.check(status == 202 and result is None, "tool notification is acknowledged without a response")

        status, result, _ = self.rpc("tools/call", {"name": "terminal.status", "arguments": {}}, modern=True)
        self.check(status >= 400 or "error" in result or result.get("result", {}).get("isError"),
                   "native terminal tools cannot bypass code_mode through public tools/call")
        workspace = self.directory / "second-workspace"
        workspace.mkdir()
        sibling, sibling_credentials = self.start_client("sibling", cwd=workspace)
        sibling_id = sibling_credentials["client_id"]
        primary_token, primary_grant = self.secret, self.oauth_grant
        self.oauth_login(sibling_credentials)
        sibling_token = self.secret
        self.secret, self.oauth_grant = primary_token, primary_grant
        inventory = self.public_call("list")
        by_id = {client["id"]: client for client in inventory["clients"]}
        self.check(set(by_id) == {self.client_id, sibling_id}
                   and by_id[sibling_id]["cwd"] == str(workspace)
                   and by_id[sibling_id]["name"] == by_id[self.client_id]["name"],
                   "multiple same-user clients with duplicate display names retain distinct IDs and directories")
        self.call("terminal.write", {"data": "pwd\n"}, client_id=sibling_id)
        self.read_until(r"[\r\n]" + re.escape(str(workspace)) + r"\r?\n", client_id=sibling_id)
        self.check(True, "code_mode routes terminal calls to the selected client ID")
        self.workspace_roundtrip(workspace, client_id=sibling_id, label="shell sibling")
        result = self.public_call("code_mode", {"code": """
            const states = [];
            for (const client of clients) {
                if (client.type === 'shell') {
                    const response = await call(client.id, 'terminal.status', {});
                    states.push({id: client.id, running: response.structuredContent.running});
                }
            }
            console.log('checked', states.length);
            return states;
        """})
        self.check(len(result["result"]) == 2 and all(state["running"] for state in result["result"])
                   and any("checked" in log for log in result["logs"]),
                   "code_mode executes JavaScript loops, conditions, awaited native calls and logs")
        result = self.public_call("code_mode", {"code": "return await tools(%s);" % json.dumps(self.client_id)})
        self.check({tool["name"] for tool in result["result"]["tools"]} == expected_tools,
                   "code_mode discovers native capability schemas through tools(client_id)")
        for code, timeout_ms, expected_error, description in (
            ("return await call('missing-client', 'terminal.status', {});", 2000,
             "client is unavailable", "unknown client IDs fail without selecting a fallback"),
            ("throw new Error('ssmcp-test-error');", 2000,
             "ssmcp-test-error", "JavaScript exceptions are returned as tool errors"),
            ("while (true) {}", 100,
             "timed out", "JavaScript CPU loops stop at the requested deadline"),
        ):
            started = time.monotonic()
            status, result, _ = self.rpc("tools/call", {"name": "code_mode", "arguments": {
                "code": code, "timeout_ms": timeout_ms,
            }}, modern=True)
            error_text = "\n".join(item.get("text", "")
                                   for item in result.get("result", {}).get("content", []))
            self.check(status == 200 and result["result"].get("isError")
                       and expected_error in error_text
                       and time.monotonic() - started < 3, description)
        self.check(self.call("terminal.status")["running"], "runtime failures leave the original client usable")
        # Local observation is independent of terminal.read and PTY echo settings.
        self.check(credentials.get("type") == "shell", "shell is the default backend")
        self.call("terminal.write", {"data": "stty -echo; printf '%s%s\\n' 'SSMCP_' 'LIVE_OUTPUT'\n"})
        log_path = self.directory / "client.log"
        wait_for(lambda: "SSMCP_LIVE_OUTPUT" in log_path.read_text(errors="replace"), description="live shell output on local stderr")
        self.check(True, "local stderr receives shell output without any MCP read")
        self.call("terminal.write", {"data": "printf '%s%s\\n' 'SSMCP_' 'ECHO_DISABLED'; stty echo\n"})
        wait_for(lambda: "SSMCP_ECHO_DISABLED" in log_path.read_text(errors="replace"), description="output with PTY echo disabled")
        transcript = log_path.read_text(errors="replace")
        self.check(transcript.count("SSMCP_ECHO_DISABLED") == 1
                   and "'ECHO_DISABLED'; stty echo" not in transcript
                   and "[ssmcp shell input]" not in transcript,
                   "local transcript shows PTY output once and respects disabled input echo")
        self.call("terminal.resize", {"rows": 37, "cols": 91})
        self.call("terminal.write", {"data": "stty size; tty; printf '%s%s:%s\\n' 'SSMCP_' 'SHELL' \"$$\"\n"})
        match, cursor, output = self.read_until(r"[\r\n]SSMCP_SHELL:(\d+)\r?\n")
        shell_pid = int(match.group(1))
        self.check("SSMCP_NOTIFICATION_EXECUTED" not in output, "tool notifications never execute terminal operations")
        self.check("37 91" in output and "/dev/pts/" in output, "resize and tty commands run in a real local PTY")
        self.call("terminal.write", {"data": "printf '%s%s\\n' '中文-' '终端'; cd /; pwd\n"})
        _, cursor, output = self.read_until(r"[\r\n]中文-终端\r?\n/\r?\n", cursor=cursor)
        self.check(True, "UTF-8 output and persistent working directory round-trip")
        self.call("terminal.write", {"data": "sleep 30\n"})
        time.sleep(0.15)
        self.call("terminal.write", {"data": "\u0003"})
        self.call("terminal.write", {"data": "printf '%s%s\\n' 'SSMCP_' 'INTERRUPTED'\n"})
        _, cursor, _ = self.read_until(r"[\r\n]SSMCP_INTERRUPTED\r?\n", cursor=cursor, timeout=3)
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
        _, cursor, _ = self.read_until(r"[\r\n]SSMCP_CONCURRENT\r?\n", cursor=cursor)

        self.call("terminal.write", {"data": "sleep 300 & printf '%s%s:%s\\n' 'SSMCP_' 'CHILD' \"$!\"\n"})
        match, cursor, _ = self.read_until(r"[\r\n]SSMCP_CHILD:(\d+)\r?\n", cursor=cursor)
        child_pid = int(match.group(1))
        self.call("terminal.write", {"data": "sleep 300\n"})
        time.sleep(0.1)
        self.stop(self.client)
        wait_for(lambda: self.rpc("ping")[0] == 401, description="revocation after client disconnect")
        self.check(True, "issuer disconnect immediately revokes its Bearer while sibling clients remain online")
        remaining = self.public_call("list", secret=sibling_token)
        self.check([client["id"] for client in remaining["clients"]] == [sibling_id]
                   and self.call("terminal.status", client_id=sibling_id,
                                 secret=sibling_token)["running"],
                   "disconnect removes only its client and preserves the sibling connection and credential")
        wait_for(lambda: not process_running(shell_pid), description="PTY shell exit")
        wait_for(lambda: not process_running(child_pid), description="background terminal job exit")
        self.check(True, "disconnect cleans up shell and terminal jobs")
        old_secret = self.secret
        old_client_id = self.client_id
        status, result, _ = self.oauth_refresh()
        self.check(status == 400 and result.get("error") == "invalid_grant", "SSH disconnect revokes OAuth refresh authority")
        self.client, credentials = self.start_client("reconnect")
        self.oauth_login(credentials)
        self.client_id = credentials["client_id"]
        self.check(self.secret != old_secret and self.request(body={}, secret=old_secret)[0] == 401,
                   "reconnect requires a new OAuth login and rejects the previous access token")
        status, result, _ = self.rpc("tools/call", {"name": "code_mode", "arguments": {
            "code": "return await call(%s, 'terminal.status', {});" % json.dumps(old_client_id),
        }}, modern=True)
        self.check(self.client_id != old_client_id and status == 200 and result["result"].get("isError"),
                   "reconnect creates a fresh ID and stale IDs cannot target the replacement")
        self.check(self.call("terminal.status")["running"], "reconnect creates a usable new terminal")
        previous_refresh = self.oauth_grant["tokens"]["refresh_token"]
        status, tokens, _ = self.oauth_refresh(resource=PUBLIC_URL + "/different-user")
        self.check(status == 400 and tokens.get("error"), "refresh tokens cannot switch the protected resource")
        status, tokens, _ = self.oauth_refresh()
        self.check(status == 200 and tokens["refresh_token"] != previous_refresh, "refresh rotates the OAuth refresh token")
        self.sensitive += [tokens["access_token"], tokens["refresh_token"]]
        self.secret = tokens["access_token"]
        self.oauth_grant["tokens"] = tokens
        self.check(self.call("terminal.status")["running"], "a refreshed OAuth access token can control the same client")
        status, result, _ = self.oauth_refresh(previous_refresh)
        self.check(status == 400 and result.get("error") == "invalid_grant", "a reused refresh token is rejected")
        self.check(self.request(body={})[0] == 401, "refresh-token replay revokes the token family")
        self.stop(self.client)
        self.client, credentials = self.start_client("code-replay")
        grant = self.oauth_login(credentials)
        self.client_id = credentials["client_id"]
        status, result, _ = self.oauth_request(grant["token_path"], "POST", grant["exchange"], form=True)
        self.check(status == 400 and result.get("error") == "invalid_grant", "an OAuth authorization code is single use")
        self.check(self.request(body={})[0] == 401, "authorization-code replay revokes its issued token family")

        self.stop(self.client)
        self.stop(sibling)


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
