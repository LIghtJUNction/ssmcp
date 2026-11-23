#!/usr/bin/env python3
"""Exercise actual clients through an isolated production account WebSocket router.

Uses only the Python standard library and an already-built Rust router fixture.
Set SSMCP_E2E_FIXTURE_BINARY to that library test executable. Only its upstream
GitHub provider is synthetic; no installed account or remote host is used.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import http.client
import hashlib
import json
import os
from pathlib import Path

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

from e2e_account_gateway import AccountGateway, direct_loopback


MODERN = "2026-07-28"
LEGACY = "2025-11-25"
PUBLIC_URL = "https://ssmcp.example"
PROTOCOL_META = "io.modelcontextprotocol/protocolVersion"
CAPABILITIES_META = "io.modelcontextprotocol/clientCapabilities"
PUBLIC_TOOLS = frozenset({"list", "code_mode", "collab", "await", "messages", "link"})

SHELL_TOOLS = frozenset({
    "terminal.read", "terminal.write", "terminal.resize", "terminal.status",
    "terminal.open", "terminal.list", "terminal.select", "terminal.close",
})
BUILTIN_TOOLS = frozenset({
    "ssmcp.fs.list", "ssmcp.fs.stat", "ssmcp.fs.read", "ssmcp.fs.write", "ssmcp.fs.mkdir",
    "ssmcp.fs.read_image", "ssmcp.fs.read_text", "ssmcp.fs.write_text", "ssmcp.fs.edit_text", "ssmcp.fs.events",
    "ssmcp.files.list", "ssmcp.files.cleanup", "ssmcp.files.config", "ssmcp.files.configure",
    "ssmcp.context.roots", "ssmcp.skills.list", "ssmcp.skills.read", "ssmcp.skills.view",
    "ssmcp.agents.list", "ssmcp.agents.read", "ssmcp.agents.resolve",
    "ssmcp.net.listen", "ssmcp.net.accept", "ssmcp.net.connect", "ssmcp.net.read",
    "ssmcp.net.write", "ssmcp.net.shutdown", "ssmcp.net.close", "ssmcp.net.list",
    "ssmcp.rsync.start", "ssmcp.rsync.read", "ssmcp.rsync.write",
    "ssmcp.rsync.status", "ssmcp.rsync.close",
    "ssmcp.client.reload", "ssmcp.client.capabilities",
    "ssmcp.resources.list", "ssmcp.resources.templates", "ssmcp.resources.read",
    "ssmcp.output.read", "ssmcp.output.stat", "ssmcp.output.cleanup",
    "ssmcp.time.now", "ssmcp.time.wait",
    "ssmcp.exec.start", "ssmcp.exec.list", "ssmcp.exec.status", "ssmcp.exec.wait",
    "ssmcp.exec.read", "ssmcp.exec.write", "ssmcp.exec.cancel", "ssmcp.exec.forget", "ssmcp.exec.events",
    "ssmcp.harnesses.list", "ssmcp.harnesses.load", "ssmcp.audit.events",
    "ssmcp.plugins.list", "ssmcp.plugins.load", "ssmcp.plugins.unload",
    "ssmcp.ui.listener",
    "ssmcp.ui.reply",
    "ssmcp.ui.wait",
    "ssmcp.ui.tool_summary",
    "ssmcp.ui.get", "ssmcp.ui.set", "ssmcp.ui.filter", "ssmcp.ui.action", "ssmcp.ui.queue", "ssmcp.ui.message", "ssmcp.ui.present", "ssmcp.ui.ask", "ssmcp.ui.questions", "ssmcp.ui.question_ack","ssmcp.ui.question_delivery", "ssmcp.ui.events", "ssmcp.ui.harness",
    "ssmcp.computer.version", "ssmcp.computer.download",
    "ssmcp.computer.install", "ssmcp.computer.uninstall",
})

WORKER_TOOLS = frozenset({
    "ssmcp.worker.status", "ssmcp.worker.attach", "ssmcp.worker.tasks",
    "ssmcp.worker.stop", "ssmcp.worker.resize", "ssmcp.worker.submit", "ssmcp.worker.result",
    "ssmcp.worker.cancel", "ssmcp.worker.wait", "ssmcp.worker.wait_available", "ssmcp.worker.wait_for", "ssmcp.worker.claim", "ssmcp.worker.complete",
    "ssmcp.worker.report", "ssmcp.worker.release",
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
        self.user = "mcp"
        self.http_port = 0
        self.origin = ""
        self.issuer = ""
        self.account_gateway = AccountGateway(self)
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
            if process is not self.account_gateway.process:
                self.stop(process)
        self.account_gateway.stop()
        for process in self.processes:
            if process.stdout:
                process.stdout.close()
        for _, log in self.logs:
            log.close()
        if self.account_gateway.prepared and self.account_gateway.exit_code != 0:
            raise AssertionError("the owned account-router fixture did not terminate successfully")

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
        connection = http.client.HTTPConnection(
            "127.0.0.1", self.http_port, timeout=getattr(self, "http_timeout", 10)
        )
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

    def native_catalog(self, client_id):
        capabilities = self.public_call("list", {"client_id": client_id})
        catalog = list(capabilities["tools"])
        cursors = set()
        while capabilities.get("nextCursor"):
            cursor = capabilities["nextCursor"]
            if cursor in cursors:
                raise AssertionError("list repeated a catalog cursor")
            cursors.add(cursor)
            capabilities = self.public_call("list", {"client_id": client_id, "cursor": cursor})
            catalog.extend(capabilities["tools"])
        return catalog

    def live_events(self, *, agent_id=None, main=False, client_id=None, limit=16,
                    cursor=None, generation=None):
        """Read the current process journal with its real scope and cursor.

        Observations must not substitute saved conversations or hide retention
        gaps. A large native page may itself be a bounded in-memory artifact.
        """
        if agent_id is not None and main:
            raise ValueError("main and agent_id are mutually exclusive")
        selected = client_id or self.client_id
        arguments = {"action": "thread", "client_id": selected, "limit": limit}
        if agent_id is not None:
            arguments["agent_id"] = agent_id
        elif main:
            arguments["main"] = True
        if cursor is not None:
            if cursor and not generation:
                raise ValueError("a nonzero event cursor requires its returned generation")
            arguments.update(cursor=cursor)
        if generation is not None:
            arguments["generation"] = generation
        expected_kind = "thread" if agent_id is not None else "main" if main else "process"
        events, seen = [], set()
        scope = None
        # A one-event page is useful while a targeted input is pending. The
        # process journal can retain 4096 events, so 128 pages is not a complete
        # upper bound for a small-page read of this bounded window.
        for _ in range(4096):
            page = self.live_event_call("ssmcp.ui.events", arguments, client_id=selected)
            if page.get("spilled"):
                page = self.read_live_native_page(page, client_id=selected)
            if (page.get("schema_version") != 1 or not isinstance(page.get("events"), list)
                    or page.get("retention") != "live_process_memory"
                    or page.get("scope", {}).get("client_id") != selected
                    or page["scope"].get("kind") != expected_kind
                    or page["scope"].get("agent_id") != agent_id
                    or not isinstance(page.get("generation"), str)):
                raise AssertionError("ui.events did not return the exact live process scope")
            if ((generation is not None and page["generation"] != generation)
                    or (scope is not None and page["scope"] != scope)):
                raise AssertionError("event paging changed the process generation or authenticated scope")
            if page.get("truncated_since_cursor") or page.get("retention_gap_possible"):
                raise AssertionError("the private fixture lost retained events before its assertions")
            generation, scope = page["generation"], page["scope"]
            for event in page["events"]:
                sequence = event.get("sequence")
                if not isinstance(sequence, int) or sequence in seen:
                    raise AssertionError("ui.events repeated or returned an invalid event sequence")
                seen.add(sequence)
                events.append(event)
            next_cursor = page["next_cursor"]
            if not page["has_more"]:
                return {**page, "events": events}
            if not isinstance(next_cursor, int) or (cursor is not None and next_cursor <= cursor):
                raise AssertionError("ui.events did not advance its real cursor")
            cursor = next_cursor
            arguments.update(cursor=cursor, generation=generation)
        raise AssertionError("the live event fixture exceeded its bounded page count")

    def live_event_call(self, name, arguments, *, client_id):
        if name not in ("ssmcp.ui.events", "ssmcp.output.read"):
            raise AssertionError("unexpected live event observation tool")
        code = "return await call(%s, %s, %s);" % (
            json.dumps(client_id), json.dumps(name), json.dumps(arguments),
        )
        outer = self.public_call("code_mode", {"code": code})
        if outer.get("user_messages") or outer.get("native_messages"):
            raise AssertionError("live event observation consumed pending fixture input")
        native = outer["result"]
        if native.get("isError"):
            raise AssertionError(f"Native event observation {name} failed: {native}")
        return native["structuredContent"]

    def read_live_native_page(self, summary, *, client_id):
        meta = summary.get("artifact", {})
        size = meta.get("bytes")
        if (summary.get("format") != "mcp_tool_result_json" or summary.get("output_error")
                or meta.get("storage") != "memory" or meta.get("complete") is not True
                or not isinstance(size, int) or not 0 < size <= 1024 * 1024
                or meta.get("observed_bytes") != size
                or not re.fullmatch(r"[0-9a-f]{32}", str(meta.get("id", "")))
                or not re.fullmatch(r"[0-9a-f]{64}", str(meta.get("sha256", "")))):
            raise AssertionError("event page is not a complete bounded memory artifact")
        # output.read is eligible work. Do not let a large observation claim a
        # pending user request while checking that UI reads are transparent.
        if hasattr(self, "queue") and any(row["status"] in ("queued", "sending")
                                          for row in self.queue()["queue"]):
            raise AssertionError("an event artifact read would consume pending fixture input")
        payload = bytearray()
        while len(payload) < size:
            offset = len(payload)
            part = self.live_event_call("ssmcp.output.read", {
                "id": meta["id"], "offset": offset, "max_bytes": min(4096, size - offset),
            }, client_id=client_id)
            chunk = base64.b64decode(part["data_base64"], validate=True)
            end = offset + len(chunk)
            if (not chunk or end > size or part.get("id") != meta["id"]
                    or part.get("offset") != offset or part.get("next_offset") != end
                    or part.get("size") != size or part.get("complete") is not True
                    or part.get("eof") is not (end == size)
                    or part.get("sha256") != hashlib.sha256(chunk).hexdigest()):
                raise AssertionError("event artifact returned an incomplete or mismatched chunk")
            payload.extend(chunk)
        if hashlib.sha256(payload).hexdigest() != meta["sha256"]:
            raise AssertionError("event artifact digest differs from its retained bytes")
        native = json.loads(payload)
        if native.get("isError") or not isinstance(native.get("structuredContent"), dict):
            raise AssertionError("event artifact did not contain a successful native response")
        return native["structuredContent"]

    def workspace_roundtrip(self, directory, *, client_id=None, label="workspace"):
        """Exercise universal file helpers over the account relay in a temporary root."""
        payload = bytes(range(256)) * 17 + b"\x00ssmcp account roundtrip\xff"
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
                   f"{label}: fs.stat identifies the account client's exact shared root")
        self.check(base64.b64decode(result["read"]["data_base64"], validate=True) == payload
                   and result["read"]["eof"] and result["stat"]["kind"] == "file"
                   and result["stat"]["size"] == len(payload)
                   and (directory / relative).read_bytes() == payload,
                   f"{label}: fs.mkdir/write/read/stat preserve binary bytes through the account relay")

        skill = "---\nname: account-skill\ndescription: Inspect files through the account relay.\n---\nSKILL_BODY_ONLY_ON_READ\n"
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
            await fs.writeText(id, {path:"skills/account-skill/SKILL.md",text:%s,create_parents:true});
            await fs.writeText(id, {path:"workspace-check/AGENTS.md",text:"Parent rule.\n"});
            await fs.writeText(id, {path:"workspace-check/nested/agents.md",text:"Nested rule.\n"});
            const roots = await context.roots(id);
            const index = await skills.list(id, {root:"workspace"});
            const view = await skills.view(id, {root:"workspace"});
            const full = await skills.read(id, {path:"skills/account-skill/SKILL.md"});
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
                   f"{label}: native text edit enforces SHA-256 preconditions through the account relay")
        self.check(result["roots"]["roots"][0]["id"] == "workspace"
                   and result["roots"]["roots"][0]["path"] == str(directory.resolve()),
                   f"{label}: context roots identify the actual account client directory")
        summaries = result["index"]["skills"]
        self.check(len(summaries) == 1 and summaries[0]["name"] == "account-skill"
                   and "SKILL_BODY_ONLY_ON_READ" not in json.dumps(result["index"])
                   and "Inspect files through the account relay." in result["view"]["markdown"],
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
        expected_client = client_id or self.client_id
        for resource_key, native_key, field, kind in (
            ("skillResource", "index", "skills", "skills"),
            ("ruleResource", "rules", "files", "agents"),
        ):
            native = result[native_key]
            expected = [
                {
                    **row,
                    "readUri": f"ssmcp://clients/{expected_client}/{kind}/read?"
                    + urlencode({"root": row.get("root", native["root"])["id"],
                                 "path": row["path"]}),
                }
                for row in native[field]
            ]
            resource = result[resource_key]
            self.check(resource["client"]["id"] == expected_client
                       and resource["root"] == native["root"]
                       and resource[field] == expected,
                       f"{label}: dynamic {kind} resource preserves source metadata "
                       "and binds content links to the exact client and root")
        resource_files = result["ruleResource"]["files"]
        native_files = result["rules"]["files"]
        self.check(len(resource_files) == len(native_files),
                   f"{label}: the instruction resource preserves the native chain length")
        for resource_file, native_file in zip(resource_files, native_files):
            parsed = urlsplit(resource_file["readUri"])
            self.check(resource_file["path"] == native_file["path"]
                       and parsed.scheme == "ssmcp" and parsed.netloc == "clients"
                       and parsed.path == f"/{expected_client}/agents/read" and not parsed.fragment
                       and parse_qs(parsed.query, keep_blank_values=True, strict_parsing=True)
                       == {"root": ["workspace"], "path": [native_file["path"]]},
                       f"{label}: instruction readUri names the exact client, root and file")
        readback_code = "return await Promise.all(%s.map(uri => resources.read(uri)));" % json.dumps(
            [row["readUri"] for row in resource_files]
        )
        readback = self.public_call("code_mode", {"code": readback_code})["result"]
        self.check(len(readback) == len(native_files)
                   and all(value["contents"] == [{
                       "uri": resource_file["readUri"], "mimeType": "text/markdown",
                       "text": native_file["content"],
                   }] for value, resource_file, native_file in zip(readback, resource_files, native_files)),
                   f"{label}: every instruction readUri returns the exact original text through the account relay")
        undecorated = []
        for resource_file in resource_files:
            native_copy = dict(resource_file)
            del native_copy["readUri"]
            undecorated.append(native_copy)
        self.check(result["skillResource"]["skills"][0]["name"] == "account-skill"
                   and undecorated == native_files,
                   f"{label}: dynamic resources preserve all native fields beside their verified readUri")

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
        self.account_gateway.prepare()

    def client_command(self, *, name="workstation", shell="/bin/sh", extra_args=()):
        return self.account_gateway.client_command(name=name, shell=shell, extra_args=extra_args)

    def validate_connection_record(self, value):
        return self.account_gateway.validate_connection_record(value)

    def connection_record(self, front, description):
        deadline = time.monotonic() + 15
        while time.monotonic() < deadline:
            front.pump(0.05)
            if front.process.poll() is not None:
                raise AssertionError(f"client exited during {description}\n{front.failure_detail()}")
            if b"\n" in front.stdout:
                line, _, rest = front.stdout.partition(b"\n")
                front.stdout = bytearray(rest)
                return self.validate_connection_record(json.loads(line))
        raise AssertionError(f"timed out during {description}\n{front.failure_detail()}")

    def external_invocations(self, label):
        """Read only this fixture client's bounded external-program invocation log."""
        path = self.directory / f"{label}-bin" / "external-invocations.jsonl"
        if not path.exists():
            return []
        if path.stat().st_size > 1024 * 1024:
            raise AssertionError("external fixture invocation log exceeded its bound")
        rows = [json.loads(line) for line in path.read_text().splitlines()]
        if not all(isinstance(row.get("provider"), str)
                   and isinstance(row.get("argv"), list)
                   and all(isinstance(argument, str) for argument in row["argv"])
                   and isinstance(row.get("pid"), int) for row in rows):
            raise AssertionError("invalid external fixture invocation record")
        return rows

    def client_environment(self, label, *, harness=None, fixture=None, fixture_args=(), harnesses=()):
        """Expose only shell utilities and the explicitly selected local fixture.

        Production loads external providers only on explicit request. Never inherit
        installed harnesses here: the tests must not start a real model account.
        Every fixture invocation is recorded before even handling --version.
        """
        directory = self.directory / f"{label}-bin"
        directory.mkdir(mode=0o700)
        (directory / "client.toml").touch(exist_ok=True)
        for name in ("stty", "tty", "sleep", "head", "tr", "git"):
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
            provider = "claude" if kind == "cc" else kind
            invocation_log = directory / "external-invocations.jsonl"
            executable = [sys.executable, str(Path(program).resolve())]
            extra = [str(argument) for argument in arguments]
            wrapper.write_text(
                f"#!{sys.executable}\n"
                "import json, os, sys\n"
                f"record = {{'provider': {provider!r}, 'argv': sys.argv[1:], 'pid': os.getpid()}}\n"
                f"fd = os.open({str(invocation_log)!r}, os.O_WRONLY | os.O_APPEND | os.O_CREAT, 0o600)\n"
                "try:\n"
                "    os.write(fd, (json.dumps(record, separators=(',', ':')) + '\\n').encode())\n"
                "finally:\n"
                "    os.close(fd)\n"
                "if sys.argv[1:2] == ['--version']:\n"
                f"    print({(names[kind] + ' fixture 2.3.0')!r})\n"
                "    raise SystemExit(0)\n"
                f"os.execv({sys.executable!r}, {executable!r} + sys.argv[1:] + {extra!r})\n"
            )
            wrapper.chmod(0o700)
        # Each child gets only the owned fixture account and isolated lifecycle
        # state. Never consult the developer's saved login or reload handoff.
        state = self.directory / f"{label}-state"
        state.mkdir(mode=0o700)
        (state / "auth").mkdir(mode=0o700)
        (state / "sessions").mkdir(mode=0o700)
        config = state / "config.toml"
        config.write_text("")
        inherited = {key: value for key, value in os.environ.items()
                     if key not in ("SSMCP_PI_RPC_SOCKET", "SSMCP_OPENCODE_URL", "OPENCODE_PASSWORD", "OPENCODE_SERVER_PASSWORD", "SSMCP_PLAYWRIGHT_MCP",
                                    "SSMCP_UI_CONFIG", "SSMCP_SESSION_GENERATION", "SSMCP_CLIENT_OUTBOX", "SSMCP_OUTPUT_RELOAD")}
        environment = direct_loopback({
            **inherited,
            "PATH": str(directory.resolve()),
            "ENV": "/dev/null",
            "BASH_ENV": "/dev/null",
            "SSMCP_AUTH_HOME": str(state / "auth"),
            "SSMCP_SESSION_HOME": str(state / "sessions"),
            "SSMCP_CLIENT_CONFIG": str(config),
            "SSMCP_UI_CONFIG": str(config),
        })
        return self.account_gateway.seed_login(environment)

    def start_client(self, label="client", *, name="workstation", cwd=None, harness=None,
                     fixture=None, fixture_args=(), destination=None, harnesses=(), extra_env=None):
        if destination is not None:
            raise ValueError("SSH destinations are retired; this fixture uses its owned account server")
        command = self.client_command(name=name)
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
        return process, self.validate_connection_record(json.loads(line))

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
        return self.account_gateway.authenticate_mcp(credentials, attacks=attacks)

    def oauth_refresh(self, refresh_token=None, resource=None):
        grant = self.oauth_grant
        return self.oauth_request(grant["token_path"], "POST", {
            "grant_type": "refresh_token", "client_id": self.oauth_client["client_id"],
            "refresh_token": refresh_token or grant["tokens"]["refresh_token"],
            "resource": resource or grant["exchange"]["resource"],
        }, form=True)

    def run(self):
        self.prepare()
        for credential in ("", "invalid-account-credential"):
            status, _, _ = self.request("GET", path="/api/client/whoami", secret=credential)
            self.check(status == 401, "the account API rejects missing or incorrect device credentials")
        status, account, _ = self.request("GET", path="/api/client/whoami", secret=self.account_token)
        self.check(status == 200 and isinstance(account, dict),
                   "the owned account credential receives an account identity response")
        self.account_gateway.validate_account_identity(account.get("identity"))
        self.client, credentials = self.start_client()
        self.client_id = credentials["client_id"]
        status, sessions, _ = self.request("GET", path="/api/client/sessions", secret=self.account_token)
        self.check(status == 200 and any(item["id"] == self.client_id for item in sessions["clients"]),
                   "the real WebSocket Ready connection belongs to the authenticated account")
        self.oauth_login(credentials, attacks=True)
        self.check(credentials["url"] == self.issuer + "/mcp", "the account connection advertises its shared MCP endpoint")
        self.check(self.request(body={}, secret="")[0] == 401, "missing Bearer is rejected")
        self.check(self.request(body={}, secret="0" * 64)[0] == 401, "incorrect Bearer is rejected")
        self.check(self.request(body={}, path="/different-user")[0] in (401, 404),
                   "an identity token cannot select an unrelated protected resource")
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
        self.check(status == 200 and {tool["name"] for tool in result["result"]["tools"]} == PUBLIC_TOOLS
                   and len(result["result"]["tools"]) == len(PUBLIC_TOOLS),
                   "legacy tools/list exposes exactly list, code_mode, collab, await, messages and link")
        inventory = self.public_call("list")
        self.check(len(inventory["clients"]) == 1
                   and inventory["clients"][0]["id"] == self.client_id
                   and inventory["clients"][0]["name"] == "workstation"
                   and inventory["clients"][0]["cwd"] == str(Path.cwd())
                   and inventory["clients"][0]["type"] == "shell"
                   and isinstance(inventory["code_mode"], dict),
                   "list returns client identity, display name, launch cwd, backend type and code API")
        catalog = self.native_catalog(self.client_id)
        expected_tools = SHELL_TOOLS | BUILTIN_TOOLS
        actual_tools = {tool["name"] for tool in catalog}
        invalid_schemas = [tool["name"] for tool in catalog
                           if not isinstance(tool.get("inputSchema"), dict)]
        self.check(actual_tools == expected_tools
                   and len(catalog) == len(expected_tools)
                   and not invalid_schemas,
                   "list discovers exact native capability names and schemas for code_mode: "
                   f"missing={sorted(expected_tools - actual_tools)}, "
                   f"extra={sorted(actual_tools - expected_tools)}, "
                   f"duplicates={len(catalog) - len(actual_tools)}, invalid_schemas={invalid_schemas}")
        self.check(any(key.lower() == "cache-control" and "no-store" in value for key, value in headers.items()),
                   "MCP responses cannot be stored by caches")
        self.check(self.rpc("ping", headers={"Origin": self.issuer})[0] == 200, "configured Origin is accepted")
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
        self.check(status == 200 and {tool["name"] for tool in result["result"]["tools"]} == PUBLIC_TOOLS
                   and len(result["result"]["tools"]) == len(PUBLIC_TOOLS),
                   "modern tools/list exposes the same public tools")
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
        result = self.public_call("code_mode", {"code": """
            const catalog = [], seen = new Set();
            let cursor;
            do {
                const page = await tools(%s, cursor);
                catalog.push(...page.tools);
                cursor = page.nextCursor;
                if (cursor && seen.has(cursor)) throw Error('catalog cursor repeated');
                if (cursor) seen.add(cursor);
            } while (cursor);
            return {tools: catalog};
        """ % json.dumps(self.client_id)})
        self.check({tool["name"] for tool in result["result"]["tools"]} == expected_tools
                   and len(result["result"]["tools"]) == len(expected_tools),
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
        wait_for(lambda: all(client["id"] != self.client_id for client in self.public_call("list")["clients"]),
                 description="removal of the disconnected WebSocket client")
        self.check(self.rpc("ping")[0] == 200,
                   "device disconnect preserves the identity-scoped MCP grant for sibling clients")
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
        self.client, credentials = self.start_client("reconnect")
        self.client_id = credentials["client_id"]
        self.check(self.secret == old_secret and self.rpc("ping", secret=old_secret)[0] == 200
                   and any(client["id"] == self.client_id for client in self.public_call("list")["clients"]),
                   "the existing account grant sees its replacement device without a fabricated login code")
        status, result, _ = self.rpc("tools/call", {"name": "code_mode", "arguments": {
            "code": "return await call(%s, 'terminal.status', {});" % json.dumps(old_client_id),
        }}, modern=True)
        self.check(self.client_id != old_client_id and status == 200 and result["result"].get("isError"),
                   "reconnect creates a fresh ID and stale IDs cannot target the replacement")
        self.check(self.call("terminal.status")["running"], "reconnect creates a usable new terminal")
        previous_refresh = self.oauth_grant["tokens"]["refresh_token"]
        status, tokens, _ = self.oauth_refresh(resource=self.issuer + "/different-user")
        self.check(status == 400 and tokens.get("error"), "refresh tokens cannot switch the protected resource")
        status, tokens, _ = self.oauth_refresh()
        self.check(status == 200 and tokens["refresh_token"] != previous_refresh,
                   "account refresh authority survives device replacement and rotates its refresh token")
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
    parser.add_argument("--fixture-binary", type=Path, default=os.environ.get("SSMCP_E2E_FIXTURE_BINARY"),
                        help="prebuilt library fixture; overrides SSMCP_E2E_FIXTURE_BINARY")
    parser.add_argument("--report", type=Path, help="write a JSON report without credentials")
    args = parser.parse_args()
    if args.fixture_binary is not None:
        fixture = args.fixture_binary.resolve()
        if not fixture.is_file() or not os.access(fixture, os.X_OK):
            parser.error(f"prebuilt fixture is not executable: {fixture}")
        os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(fixture)
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
            "transport": "production Broker/router fixture + loopback account HTTP/WS + owned local PTY",
            "oauth": "real discovery/DCR/PKCE/consent with a synthetic upstream GitHub provider",
            "tls_verified": False, "real_github_account": False, "models_started": False,
            "fixture_exit_code": harness.fixture_exit_code,
        }
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(f"\nPassed {len(harness.checks)} end-to-end checks in {report['duration_seconds']}s")


if __name__ == "__main__":
    main()
