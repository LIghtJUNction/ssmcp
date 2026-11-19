#!/usr/bin/env python3
"""Exercise native-only startup and explicit harness loading through local WSS/MCP/OAuth.

Shell uses a real PTY. Harnesses use deterministic local protocol fixtures;
no installed agent, model account, real identity or paid API is used.
Run after cargo build --locked, with SSMCP_E2E_FIXTURE_BINARY set to the lib test binary.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import json
import os
import re
from pathlib import Path
import secrets
import selectors
import socket
import subprocess
import sys
import tempfile
import threading
import time

from e2e import BUILTIN_TOOLS, SHELL_TOOLS, Harness, process_running, wait_for


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests" / "fixtures"
MODES = ("shell", "codex", "cc", "pi", "opencode")


class ExistingServices:
    """Test-owned native services, initialized before the client can attach.

    Pi's socket only forwards an already running JSONL session. It never starts
    a process on connect. OpenCode's HTTP fixture likewise runs independently
    of ssmcp. Fixture-only PATH wrappers reject any accidental automatic launch.
    """

    def __init__(self, harness, directory):
        self.harness = harness
        self.directory = directory
        self.environment = {}
        self.processes = []
        self.stopping = threading.Event()
        self.listener = None
        self.connection = None
        self.thread = None
        self.relay_error = None
        self.guard = directory / "reject-automatic-launch.py"
        self.attempts = directory / "automatic-launch-attempted"
        self.guard.write_text(
            "from pathlib import Path\n"
            f"Path({str(self.attempts)!r}).write_text('unexpected native launch')\n"
            "raise SystemExit(91)\n"
        )
        harness.services.append(self)

    def spawn(self, kind, directory, arguments, environment):
        log_path = directory / "external-service.log"
        log = log_path.open("w+")
        self.harness.logs.append((log_path, log))
        process = subprocess.Popen(
            [sys.executable, str(FIXTURES / kind), *arguments],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log,
            bufsize=0, cwd=directory, env={**os.environ, **environment},
            start_new_session=True,
        )
        self.processes.append(process)
        self.harness.processes.append(process)
        return process

    @staticmethod
    def read_json(process):
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            if not selector.select(timeout=5):
                raise AssertionError("external fixture did not initialize")
        line = process.stdout.readline()
        if not line:
            raise AssertionError("external fixture exited before initialization")
        return json.loads(line)

    def start_pi(self, directory):
        process = self.spawn("pi_rpc.py", directory, ["--mode", "rpc", "--offline"], {"PI_OFFLINE": "1"})
        process.stdin.write(b'{"id":"fixture-preflight","type":"get_state"}\n')
        state = self.read_json(process)
        self.harness.check(state.get("success") is True
                           and state.get("command") == "get_state"
                           and state.get("data", {}).get("pid") == process.pid,
                           "pi: native fixture session is initialized before publishing its socket")
        path = directory / "native.sock"
        self.listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.listener.bind(str(path))
        path.chmod(0o600)
        self.listener.listen(1)
        self.listener.settimeout(0.1)
        self.thread = threading.Thread(target=self.relay_pi, args=(process,), daemon=True)
        self.thread.start()
        self.environment["SSMCP_PI_RPC_SOCKET"] = str(path)

    def relay_pi(self, process):
        try:
            while not self.stopping.is_set():
                try:
                    connection, _ = self.listener.accept()
                except socket.timeout:
                    continue
                self.connection = connection
                try:
                    with connection, selectors.DefaultSelector() as selector:
                        connection.settimeout(1)
                        selector.register(connection, selectors.EVENT_READ)
                        selector.register(process.stdout, selectors.EVENT_READ)
                        connected = True
                        while connected and not self.stopping.is_set():
                            for key, _ in selector.select(timeout=0.1):
                                if key.fileobj is connection:
                                    data = connection.recv(65536)
                                    if not data:
                                        connected = False
                                        break
                                    # Unbuffered pipes can perform a partial write.
                                    remaining = memoryview(data)
                                    while remaining:
                                        remaining = remaining[process.stdin.write(remaining):]
                                else:
                                    data = os.read(process.stdout.fileno(), 65536)
                                    if not data:
                                        raise AssertionError("external Pi fixture closed its native stream")
                                    connection.sendall(data)
                except (BrokenPipeError, ConnectionResetError):
                    # Disconnect closes only this attachment, not Pi's stdin.
                    pass
                finally:
                    self.connection = None
        except Exception as error:
            if not self.stopping.is_set():
                self.relay_error = error

    def start_opencode(self, directory):
        password = secrets.token_hex(24)
        self.harness.sensitive.extend([
            password, base64.b64encode(f"opencode:{password}".encode()).decode(),
        ])
        process = self.spawn("opencode_http.py", directory, [
            "serve", "--stdio", "--hostname", "127.0.0.1", "--port", "0",
            "--fixture-dir", str(directory),
        ], {"OPENCODE_PASSWORD": password})
        endpoint = self.read_json(process)["url"]
        self.environment.update(SSMCP_OPENCODE_URL=endpoint, OPENCODE_PASSWORD=password)

    def check_no_launch(self):
        self.harness.check(not self.attempts.exists() and self.relay_error is None,
                           "external services: discovery never launches the guarded Pi/OpenCode executables")

    def close(self):
        self.stopping.set()
        connection = self.connection
        if connection is not None:
            try:
                connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
        if self.listener is not None:
            self.listener.close()
        if self.thread is not None:
            self.thread.join(timeout=2)
        for process in self.processes:
            self.harness.stop(process)
            process.stdin.close()


class ServiceHarness(Harness):
    def __init__(self, *args):
        super().__init__(*args)
        self.services = []

    def cleanup(self):
        try:
            for service in reversed(self.services):
                service.close()
        finally:
            super().cleanup()


def full_call(harness, name, arguments=None, *, expect_error=False):
    code = "return await call(%s, %s, %s);" % (
        json.dumps(harness.client_id), json.dumps(name), json.dumps(arguments or {}),
    )
    status, value, _ = harness.rpc(
        "tools/call", {"name": "code_mode", "arguments": {"code": code}}, modern=True
    )
    if status != 200 or not isinstance(value, dict) or "error" in value:
        raise AssertionError(f"{name}: expected an HTTP 200 MCP tool result")
    outer = value.get("result")
    if not isinstance(outer, dict) or outer.get("resultType") != "complete" or (
        outer.get("_meta", {}).get("io.modelcontextprotocol/serverInfo", {}).get("name") != "ssmcpd"
    ):
        raise AssertionError(f"{name}: code_mode lost modern MCP response metadata")
    if outer.get("isError"):
        if expect_error:
            return outer
        raise AssertionError(f"{name}: code_mode failed: {outer}")
    result = outer.get("structuredContent", {}).get("result")
    if not isinstance(result, dict) or bool(result.get("isError")) != expect_error:
        raise AssertionError(f"{name}: unexpected MCP error status")
    return result


def pi_call(harness, command, arguments=None):
    return harness.call("pi.rpc", {"type": command, "args": arguments or {}})


def native_defaults(harness, label, context):
    rows = harness.call("ssmcp.harnesses.list")["harnesses"]
    names = {"codex", "claude", "pi", "opencode", "playwright"}
    providers = {row["provider"]: row for row in rows}
    harness.check(len(rows) == len(providers) == 5 and set(providers) == names,
                  f"{context}: all five optional providers have distinct lazy state records")
    harness.check(all(row["status"] == "not_loaded" and row["tools"] == 0
                      and "executable" in row and row["executable"] is None
                      and "version" in row and row["version"] is None for row in rows),
                  f"{context}: startup and provider listing neither load nor probe optional programs")
    harness.check(harness.external_invocations(label) == [],
                  f"{context}: startup and observation execute zero external argv, including --version")
    full_call(harness, "unknown.fixture", expect_error=True)
    full_call(harness, "codex.definitely_missing", expect_error=True)
    harness.check(harness.external_invocations(label) == [],
                  f"{context}: unknown and unloaded prefixed tools cannot trigger discovery")
    return providers


def catalog(harness, mode, label):
    harness_names = ("codex", "claude", "pi", "opencode")
    expected = ({"codex", "claude", "pi", "opencode"} if mode == "all"
                else ({"claude" if mode == "cc" else mode} if mode != "shell" else set()))
    native_defaults(harness, label, mode)
    for provider in sorted(expected):
        harness.call("ssmcp.harnesses.load", {"provider": provider})
    diagnostics = harness.call("ssmcp.harnesses.list")["harnesses"]
    providers = {row["provider"]: row for row in diagnostics}
    harness.check({row["provider"] for row in diagnostics if row["status"] == "ready"} == expected,
                  f"{mode}: explicit load activates exactly the requested fixture providers")
    harness.check(all(row["status"] == "not_loaded" and row["tools"] == 0
                      and row.get("executable") is None and row.get("version") is None
                      for row in diagnostics if row["provider"] not in expected),
                  f"{mode}: unrelated providers remain unprobed after explicit loading")
    status, value, _ = harness.rpc("tools/list", modern=True)
    harness.check(status == 200 and [tool["name"] for tool in value["result"]["tools"]] == ["list", "code_mode", "collab"],
                  f"{mode}: public MCP catalog contains exactly list, code_mode and collab")
    inventory = harness.public_call("list")
    harness.check(len(inventory["clients"]) == 1
                  and inventory["clients"][0]["id"] == harness.client_id
                  and inventory["clients"][0]["type"] == "shell",
                  f"{mode}: list discovers the shell client with installed harness tools")
    tools = []
    cursor = None
    seen = set()
    pages = 0
    while True:
        parameters = {"client_id": harness.client_id}
        if cursor is not None:
            parameters["cursor"] = cursor
        page = harness.public_call("list", parameters)
        rows = page["tools"]
        pages += 1
        harness.check(isinstance(rows, list) and len(rows) <= 64,
                      f"{mode}: list capability page {pages} respects the 64-tool boundary")
        tools.extend(rows)
        cursor = page.get("nextCursor")
        if cursor is None:
            break
        if not isinstance(cursor, str) or not cursor or cursor in seen or pages >= 100:
            raise AssertionError(f"{mode}: invalid or repeated capability cursor")
        seen.add(cursor)
    names = [tool["name"] for tool in tools]
    harness.check(bool(names) and len(names) == len(set(names)),
                  f"{mode}: complete tool catalog has unique names")
    kinds = MODES[1:] if mode == "all" else (mode,)
    prefixes = tuple(prefix for kind in kinds for label in ("claude" if kind == "cc" else kind,)
                     for prefix in (f"{label}.", f"ssmcp.{label}."))
    shared = set(names) - SHELL_TOOLS - {
        name for name in names if name.startswith(prefixes)
    }
    harness.check(shared == BUILTIN_TOOLS,
                  f"{mode}: exact canonical shared builtins are fully discoverable")
    native = set(names) - BUILTIN_TOOLS - SHELL_TOOLS
    harness.check(SHELL_TOOLS <= set(names)
                  and (not native if mode == "shell" else bool(native) and all(name.startswith(prefixes) for name in native)),
                  f"{mode}: shell capabilities coexist with the installed harness catalog")
    harness.check(all(isinstance(tool.get("inputSchema"), dict) for tool in tools),
                  f"{mode}: all discovered tools retain JSON schemas")
    event_schema = next(tool["inputSchema"] for tool in tools if tool["name"] == "ssmcp.exec.events")
    harness.check(set(event_schema["properties"]) == {"cursor", "max_events", "wait_ms"}
                  and all(parameter.get("type") == "integer" for parameter in event_schema["properties"].values()),
                  f"{mode}: command event schema exposes cursor, limit and wait parameters directly")
    event_page = harness.call("ssmcp.exec.events", {"max_events": 1, "wait_ms": 0})
    harness.check(isinstance(event_page["events"], list) and len(event_page["events"]) <= 1
                  and isinstance(event_page["next_cursor"], int),
                  f"{mode}: command events accept the discovered parameters")
    harness.check(harness.call("terminal.status")["running"], f"{mode}: the shell stays available")
    counts = {name: providers[name]["tools"] for name in harness_names}
    harness.check(counts == {
        name: sum(tool.startswith((f"{name}.", f"ssmcp.{name}.")) for tool in names)
        for name in harness_names
    }, f"{mode}: harness counts match the complete registered catalog")
    playwright = providers["playwright"]
    harness.check(playwright["status"] == "not_loaded" and playwright["tools"] == 0
                  and not any(name.startswith("playwright.") for name in names),
                  f"{mode}: the unloaded optional Playwright MCP provider contributes no native tools")
    harness.check({name for name, count in counts.items() if count} == expected,
                  f"{mode}: explicit loading reports exactly the requested fixture harnesses")
    invocations = harness.external_invocations(label)
    full_call(harness, "unknown.fixture", expect_error=True)
    harness.check(harness.external_invocations(label) == invocations,
                  f"{mode}: an unknown tool never scans or starts another provider")
    return tools, pages


def local_log(harness, label):
    return (harness.directory / f"{label}.log").read_text(errors="replace")


def log_has_line(harness, label, expected):
    # Default client output is rendered text; structured native events are checked separately.
    text = re.sub(r"\x1b\[[0-?]*[ -/]*[@-~]", "", local_log(harness, label))
    return any(line.strip() == expected for line in text.splitlines())


def events_until(harness, tool, predicate, *, cursor=0, timeout=5):
    deadline = time.monotonic() + timeout
    events = []
    while time.monotonic() < deadline:
        page = harness.call(tool, {"cursor": cursor, "max_events": 256, "wait_ms": 100})
        if page["truncated"]:
            raise AssertionError(f"{tool}: small test unexpectedly exhausted the event cache")
        events.extend(entry["event"] for entry in page["events"])
        cursor = page["next_cursor"]
        if predicate(events):
            return events, cursor
    raise AssertionError(f"{tool}: expected native event did not arrive")


def pending_request(harness, tool):
    def first():
        requests = harness.call(tool)["requests"]
        return requests[0] if requests else None
    return wait_for(first, description=f"pending request from {tool}")


def check_shell(harness, label, tools, _directory):
    harness.check({tool["name"] for tool in tools} == SHELL_TOOLS | BUILTIN_TOOLS,
                  "shell: four native PTY capabilities plus exact shared builtins are available through code_mode")
    harness.call("terminal.write", {"data": (
        "stty -echo; printf 'SSMCP_SHELL_PID:%s\\n' \"$$\"; "
        "sleep 60 & printf 'SSMCP_CHILD_PID:%s\\n' \"$!\"; "
        "printf '%s%s\\n' 'SSMCP_SHELL_' 'LIVE_BEFORE_READ'\n"
    )})
    wait_for(lambda: "SSMCP_SHELL_LIVE_BEFORE_READ" in local_log(harness, label),
             description="shell output on local stderr before terminal.read")
    harness.check(True, "shell: local stderr receives command output before any terminal.read")
    match, _, _ = harness.read_until(
        r"SSMCP_SHELL_PID:(\d+)[\s\S]*?SSMCP_CHILD_PID:(\d+)"
    )
    transcript = local_log(harness, label)
    harness.check(transcript.count("SSMCP_SHELL_LIVE_BEFORE_READ") == 1
                  and "[ssmcp shell input]" not in transcript,
                  "shell: local transcript contains PTY output without duplicate input logging")
    harness.check(harness.call("terminal.status")["running"],
                  "shell: status reports the live PTY")
    return [int(match.group(1)), int(match.group(2))]


def check_codex(harness, label, tools, _directory):
    names = {tool["name"] for tool in tools}
    harness.check("codex.thread_start" in names and "codex.initialize" not in names,
                  "codex: native methods are discovered and initialize remains bridge-owned")
    schema = next(tool["inputSchema"] for tool in tools if tool["name"] == "codex.thread_start")
    harness.check("Node" in schema.get("definitions", {}),
                  "codex: generated recursive schema definitions survive MCP discovery")
    arguments = {"cwd": str(harness.directory), "future": {"nested": [1, "中文", True]}}
    thread = harness.call("codex.thread_start", arguments)
    harness.check(thread["thread"]["id"] == "thread-1" and thread["params"] == arguments,
                  "codex: native request parameters and result fields round-trip")
    unit = harness.call("codex.configRequirements_read")
    harness.check(unit == {"requirements": None}, "codex: no-argument native methods retain null params")
    future = harness.call("codex.rpc", {"method": "fixture/new-method", "params": {"keep": [3, 5]}})
    harness.check(future == {"native_method": "fixture/new-method", "params": {"keep": [3, 5]}},
                  "codex: generic RPC forwards future native methods")
    harness.call("codex.turn_start", {"threadId": "thread-1", "input": []})
    wait_for(lambda: "live-before-approval" in local_log(harness, label),
             description="Codex native delta on local stderr")
    harness.check(True, "codex: native output reaches local stderr before event polling")
    request = pending_request(harness, "ssmcp.codex.requests")
    diagnostics = harness.call("codex.server_diagnostics")
    harness.check(request["method"] == "item/commandExecution/requestApproval"
                  and diagnostics["answers"] == [], "codex: native approval remains unanswered until an explicit reply")
    harness.call("ssmcp.codex.respond", {"id": request["id"], "result": {"decision": "decline"}})
    events_until(harness, "ssmcp.codex.events", lambda events: any(
        event.get("method") == "turn/completed"
        and event["params"].get("decision") == {"decision": "decline"}
        for event in events
    ))
    harness.check(True, "codex: explicit decline and native turn completion round-trip")
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        running = pool.submit(harness.call, "codex.command_exec", {"command": ["hold"]})
        try:
            wait_for(lambda: log_has_line(harness, label, "live"),
                     description="Codex base64 output decoded on local stderr")
            harness.check(not running.done(), "codex: command bytes are observable while its native call is still running")
        finally:
            harness.call("codex.command_exec_terminate", {"processId": "held"})
        harness.check(running.result(timeout=5)["exitCode"] == 137,
                      "codex: concurrent termination resolves the correct pending native call")
    return [diagnostics["pid"]]


def check_cc(harness, label, tools, directory):
    native = json.loads((FIXTURES / "cc-catalog.json").read_text())[0]
    expected = {**native, "name": "claude." + native["name"]}
    harness.check(next(tool for tool in tools if tool["name"] == expected["name"]) == expected,
                  "cc: native schema, annotations and vendor catalog fields are unchanged")
    arguments = {"nested": [3, 5, 8], "spawn_child": True}
    result = full_call(harness, "claude.NativeFutureTool", arguments)
    context = result.get("_meta", {}).get("ssmcp.context")
    workspace = context.get("workspace") if isinstance(context, dict) else None
    harness.check(isinstance(context, dict) and context.get("schema_version") == 1
                   and type(context.get("revision")) is int and context["revision"] >= 1
                   and isinstance(workspace, dict) and workspace.get("path") == str(directory.resolve())
                   and isinstance(workspace.get("id"), str) and re.fullmatch(r"[0-9a-f]{64}", workspace["id"]),
                   "cc: the native result includes versioned context for this fixture workspace")
    native_result = {**result, "_meta": dict(result["_meta"])}
    del native_result["_meta"]["ssmcp.context"]
    harness.check(native_result == {
        "content": [
            {"type": "text", "text": "native result"},
            {"type": "image", "mimeType": "image/png", "data": "AAEC"},
            {"type": "resource", "resource": {
                "uri": "fixture://native-result", "mimeType": "text/plain", "text": "embedded",
            }},
        ],
        "structuredContent": {"future": {"arguments": arguments}},
        "isError": False, "_meta": {"vendor/result": [1, 2, 3]},
        "futureResultField": {"keep": "this too"},
    }, "cc: full native MCP content, image, resource, metadata and future fields survive WSS")
    error = full_call(harness, "claude.NativeFutureTool", {"error": True}, expect_error=True)
    harness.check(error["structuredContent"]["future"]["arguments"] == {"error": True},
                  "cc: native tool errors retain their structured result")
    wait_for(lambda: "fixture progress" in local_log(harness, label),
             description="Claude native progress on local stderr")
    harness.check(True, "cc: native progress appears on local stderr before event polling")
    full_call(harness, "claude.Bash", {"fixture_mode": "background"})
    wait_for(lambda: log_has_line(harness, label, "first")
             and log_has_line(harness, label, "second"),
             description="Claude background output on local stderr")
    events, _ = events_until(harness, "ssmcp.claude.events", lambda values: b"second\n" in b"".join(
        base64.b64decode(event["data_base64"])
        for event in values if event.get("type") == "background_output"
    ))
    output = b""
    for event in events:
        if event.get("type") == "background_output":
            harness.check(event["offset"] == len(output), "cc: background output preserves its byte offset")
            output += base64.b64decode(event["data_base64"])
    harness.check(output == b"first\nsecond\n", "cc: background output is incremental and preserves exact bytes")
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        running = pool.submit(full_call, harness, "claude.NeedsApproval")
        request = pending_request(harness, "ssmcp.claude.requests")
        received = [json.loads(line) for line in (directory / "received.jsonl").read_text().splitlines()]
        harness.check(not running.done() and not any(
            item.get("id") == request["id"] and ("result" in item or "error" in item) for item in received
        ), "cc: native elicitation is never automatically approved")
        harness.call("ssmcp.claude.respond", {"id": request["id"], "result": {"action": "decline"}})
        reply = running.result(timeout=5)
        harness.check(reply["structuredContent"]["approval"]["result"] == {"action": "decline"},
                      "cc: an explicit elicitation response reaches the native process")
    return [int((directory / name).read_text()) for name in ("pid", "child-pid")]


def check_pi(harness, label, tools, _directory):
    harness.check({tool["name"] for tool in tools} - BUILTIN_TOOLS - SHELL_TOOLS
                  == {"pi.rpc", "ssmcp.pi.events", "ssmcp.pi.requests", "ssmcp.pi.respond"},
                  "pi: only the native RPC bridge and three protocol controls are advertised")
    state = pi_call(harness, "get_state")
    harness.check(state["success"] and state["command"] == "get_state", "pi: full native response envelope is preserved")
    future = harness.call("pi.rpc", {"type": "future_control", "args": {"future": {"nested": [2, 7]}}})
    harness.check(future["data"] == {"future": {"nested": [2, 7]}}, "pi: generic RPC preserves future native fields")
    error = full_call(harness, "pi.rpc", {"type": "future_failure", "args": {}}, expect_error=True)
    harness.check(error["structuredContent"]["success"] is False
                  and error["structuredContent"]["error"] == "native fixture rejection",
                  "pi: native failures retain their envelope and MCP error flag")
    accepted = pi_call(harness, "prompt", {"message": "fixture prompt"})
    harness.check(accepted["data"]["disposition"] == "started", "pi: prompt acceptance is returned separately from completion")
    wait_for(lambda: log_has_line(harness, label, "agent_settled"),
             description="Pi settled event on local stderr")
    harness.check(True, "pi: native completion reaches local stderr before event polling")
    events_until(harness, "ssmcp.pi.events", lambda events: any(
        event.get("type") == "agent_settled" for event in events
    ))
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        running = pool.submit(pi_call, harness, "prompt", {"message": "/dialog confirm"})
        request = pending_request(harness, "ssmcp.pi.requests")
        harness.check(not running.done() and pi_call(harness, "get_state")["data"]["answers"] == 0,
                      "pi: extension UI waits for an explicit answer")
        harness.call("ssmcp.pi.respond", {"id": request["id"], "confirmed": False})
        reply = running.result(timeout=5)
        harness.check(reply["data"]["answer"]["confirmed"] is False,
                      "pi: explicit extension-UI rejection reaches the native process")
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        running = pool.submit(pi_call, harness, "bash", {"command": "fixture-held-command"})
        try:
            wait_for(lambda: log_has_line(harness, label, "first"),
                     description="Pi bash delta on local stderr")
            harness.check(not running.done(), "pi: Bash output is visible before the native command returns")
        finally:
            pi_call(harness, "abort_bash")
        reply = running.result(timeout=5)
        harness.check(reply["data"]["cancelled"] is True and reply["data"]["exitCode"] == 130,
                      "pi: abort remains usable while a native command is pending")
    return [state["data"]["pid"]]


def check_opencode(harness, label, tools, directory):
    names = {tool["name"] for tool in tools}
    harness.check(len(tools) == 78 + len(BUILTIN_TOOLS) + len(SHELL_TOOLS) and "opencode.fixture.extra69" in names,
                  "opencode: all native OpenAPI operations survive multi-page MCP discovery")
    def native_requests():
        return [json.loads(line) for line in (directory / "requests.jsonl").read_text().splitlines()]
    requests = native_requests()
    harness.check(bool(requests) and all(
        request["method"] == "GET" and request["path"] in ("/api/info", "/openapi.json")
        for request in requests
    ), "opencode: discovery only reads native info and OpenAPI, without starting an event session")
    info = harness.call("opencode.server.info")
    process = info["body"]
    harness.check(info["status"] == 200 and process["authenticated"]
                  and info["headers"]["x-fixture-result"] == "preserved",
                  "opencode: private authenticated HTTP responses retain status and headers")
    schema = harness.call("opencode.schema", {"operation_id": "fixture.echo"})
    harness.check(schema["operation"]["x-fixture-extension"] == {"keep": [1, 2, 3]}
                  and "EchoInput" in schema["components"]["schemas"],
                  "opencode: raw native operation extensions and referenced schemas are available")
    wait_for(lambda: any(request["path"] == "/api/event" for request in native_requests()),
             description="OpenCode observation stream after an explicit native call")
    # The fixture records subscriber registration before sending SSE headers,
    # independently of how the terminal renders an unfamiliar native event.
    wait_for(lambda: (directory / "subscriber-ready").exists(),
             description="OpenCode subscriber ready before publishing the echo")
    payload = {"message": "中文 HTTP echo", "future": {"nested": [7, True]}}
    result = harness.call("opencode.fixture.echo", {
        "path": {"itemID": "item_roundtrip"}, "query": {"trace": "round trip"},
        "headers": {"X-Fixture-In": "request-preserved"}, "body": payload,
    })
    harness.check(result["status"] == 201 and result["truncated"] is False and result["body"] == {
        "item": "item_roundtrip", "query": {"trace": ["round trip"]},
        "header": "request-preserved", "payload": payload,
        "futureResult": {"keep": [1, {"nested": True}]},
    }, "opencode: path, query, headers, Unicode body and unknown result fields round-trip")
    wait_for(lambda: "SSMCP_OPENCODE_SSE_BEFORE_READ" in local_log(harness, label),
             description="native OpenCode SSE event on local stderr")
    harness.check(True, "opencode: SSE reaches local stderr before any MCP event read")
    events_until(harness, "opencode.events", lambda events: any(
        event.get("type") == "opencode.event"
        and event.get("event", {}).get("type") == "fixture.echoed"
        for event in events
    ))
    harness.check(harness.call("opencode.fixture.extra69")["body"] == {"index": 69},
                  "opencode: an operation from the later catalog page is callable")
    requests = native_requests()
    harness.check(all(request["authenticated"] for request in requests),
                  "opencode: schema, event and API requests all use the private Basic credential")
    return [process["pid"], process["child_pid"]]


CHECKERS = {
    "shell": check_shell, "codex": check_codex, "cc": check_cc,
    "pi": check_pi, "opencode": check_opencode,
}


def native_only_and_cached_failure(harness):
    started = time.monotonic()
    before = len(harness.checks)
    label = "client-native-lazy"
    directory = harness.directory / "fixture-native-lazy"
    directory.mkdir(mode=0o700)
    rejected = directory / "reject-provider.py"
    rejected.write_text("import sys\nprint('fixture: deliberate initialization failure', file=sys.stderr)\nraise SystemExit(91)\n")
    installed = [("codex", FIXTURES / "codex_rpc.py", ())]
    installed.extend((kind, rejected, ()) for kind in ("cc", "pi", "opencode", "playwright"))
    client, credentials = harness.start_client(
        label, name="native-lazy-fixture", cwd=directory, harnesses=installed,
        extra_env={"SSMCP_PLAYWRIGHT_MCP": str(harness.directory / f"{label}-bin" / "playwright-mcp")},
    )
    harness.client = client
    harness.client_id = credentials["client_id"]
    harness.oauth_login(credentials)
    base_tools, pages = catalog(harness, "shell", label)
    harness.call("ssmcp.client.capabilities")
    original = "native fixture 中文\n"
    (directory / "native-only.txt").write_text(original)
    text = harness.call("ssmcp.fs.read_text", {"path": "native-only.txt", "max_bytes": 128})
    harness.check(text["text"] == original, "native-only: filesystem text remains readable before any optional load")
    job = harness.call("ssmcp.exec.start", {
        "command": ["/bin/sh", "-c", "printf native-only"], "cwd": str(directory), "timeout_ms": 10000,
    })
    def completed():
        result = harness.call("ssmcp.exec.read", {"job_id": job["job_id"], "wait_ms": 0})
        return result if not result["running"] else None
    result = wait_for(completed, timeout=10, description="native-only independent command")
    harness.check(result["exit_code"] == 0 and result["stdout"] == "native-only",
                  "native-only: exec starts and completes without an external provider")
    full_call(harness, "ssmcp.harnesses.load", {"provider": "unknown"}, expect_error=True)
    native_defaults(harness, label, "native-only after catalog/capabilities/exec/fs")
    harness.check(harness.external_invocations(label) == [],
                  "native-only: installed optional programs receive zero argv after native I/O and invalid loading")

    full_call(harness, "ssmcp.harnesses.load", {"provider": "claude"}, expect_error=True)
    first_calls = harness.external_invocations(label)
    harness.check(bool(first_calls) and {row["provider"] for row in first_calls} == {"claude"}
                  and any(row["argv"] == ["--version"] for row in first_calls)
                  and any(row["argv"] != ["--version"] for row in first_calls),
                  "lazy failure: only explicitly requested Claude is version-probed and started")
    first_rows = {row["provider"]: row for row in harness.call("ssmcp.harnesses.list")["harnesses"]}
    failure = first_rows["claude"]
    harness.check(failure["status"] == "unavailable" and failure["tools"] == 0
                  and isinstance(failure.get("reason"), str) and bool(failure["reason"]),
                  "lazy failure: the requested provider reports its actual initialization error")
    harness.check(all(row["status"] == "not_loaded" and row["tools"] == 0
                      and row.get("version") is None and row.get("executable") is None
                      for name, row in first_rows.items() if name != "claude"),
                  "lazy failure: unrelated optional providers remain unprobed")
    full_call(harness, "ssmcp.harnesses.load", {"provider": "claude"}, expect_error=True)
    cached_rows = {row["provider"]: row for row in harness.call("ssmcp.harnesses.list")["harnesses"]}
    harness.check(harness.external_invocations(label) == first_calls
                  and cached_rows["claude"]["reason"] == failure["reason"],
                  "lazy failure: repeated load and list reuse the failed attempt without another argv")
    full_call(harness, "unknown.fixture", expect_error=True)
    full_call(harness, "codex.definitely_missing", expect_error=True)
    harness.check(harness.external_invocations(label) == first_calls,
                  "lazy failure: unknown tools do not scan or retry providers")
    full_call(harness, "ssmcp.harnesses.load", {"provider": "claude", "refresh": True}, expect_error=True)
    retried = harness.external_invocations(label)
    attempts = lambda rows: sum(row["provider"] == "claude" and row["argv"] != ["--version"] for row in rows)
    harness.check(attempts(retried) == attempts(first_calls) + 1
                  and {row["provider"] for row in retried} == {"claude"},
                  "lazy failure: explicit refresh makes exactly one new requested initialization attempt")
    harness.check(harness.call("terminal.status")["running"]
                  and harness.call("ssmcp.fs.read_text", {"path": "native-only.txt", "max_bytes": 128})["text"] == original,
                  "lazy failure: native shell and filesystem remain available")

    harness.call("ssmcp.harnesses.load", {"provider": "codex"})
    ready = {row["provider"]: row for row in harness.call("ssmcp.harnesses.list")["harnesses"]}
    harness.check(ready["codex"]["status"] == "ready" and ready["codex"]["tools"] > 0
                  and ready["claude"]["status"] == "unavailable",
                  "lazy isolation: a healthy provider loads independently of another provider's cached failure")
    diagnostics = harness.call("codex.server_diagnostics")
    harness.check(diagnostics["model_requests"] == 0, "lazy isolation: protocol loading invokes no model")
    current, cursor, seen = [], None, set()
    while True:
        args = {"client_id": harness.client_id}
        if cursor is not None:
            args["cursor"] = cursor
        page = harness.public_call("list", args)
        current.extend(page["tools"])
        cursor = page.get("nextCursor")
        if cursor is None:
            break
        if cursor in seen or len(seen) >= 100:
            raise AssertionError("lazy isolation: invalid repeated catalog cursor")
        seen.add(cursor)
    baseline = {tool["name"]: tool for tool in base_tools}
    registered = {tool["name"]: tool for tool in current}
    added = set(registered) - set(baseline)
    harness.check(all(registered.get(name) == descriptor for name, descriptor in baseline.items())
                  and bool(added) and all(name.startswith(("codex.", "ssmcp.codex.")) for name in added),
                  "lazy isolation: catalog increments preserve every native descriptor and add only real Codex routes")
    before_observation = harness.external_invocations(label)
    harness.call("ssmcp.harnesses.list")
    harness.call("ssmcp.harnesses.load", {"provider": "codex"})
    harness.check(harness.external_invocations(label) == before_observation,
                  "lazy isolation: ready-provider observations and repeated load do not restart or re-probe")
    owned_pids = [harness.call("terminal.status")["pid"], diagnostics["pid"]]
    harness.stop(client)
    wait_for(lambda: all(not process_running(pid) for pid in owned_pids), description="lazy fixture cleanup")
    harness.secret = ""
    return {"mode": "native-only-and-cached-failure", "tools": len(current), "catalog_pages": pages,
            "checks": len(harness.checks) - before, "duration_seconds": round(time.monotonic() - started, 3)}


def unconfigured_external_services(harness):
    started = time.monotonic()
    before = len(harness.checks)
    directory = harness.directory / "fixture-unconfigured"
    directory.mkdir(mode=0o700)
    services = ExistingServices(harness, directory)
    client, credentials = harness.start_client(
        "client-unconfigured", name="unconfigured-fixture", cwd=directory,
        harnesses=[(kind, services.guard, ()) for kind in ("pi", "opencode")],
    )
    harness.client = client
    harness.client_id = credentials["client_id"]
    harness.oauth_login(credentials)
    tools, pages = catalog(harness, "shell", "client-unconfigured")
    for provider in ("pi", "opencode"):
        full_call(harness, "ssmcp.harnesses.load", {"provider": provider}, expect_error=True)
    diagnostics = harness.call("ssmcp.harnesses.list")["harnesses"]
    harness.check(all(row["status"] == "unavailable" and row["tools"] == 0
                      and isinstance(row.get("reason"), str) and row["reason"]
                      for row in diagnostics if row["provider"] in ("pi", "opencode")),
                  "external services: explicit loading without existing endpoints fails clearly")
    services.check_no_launch()
    pid = harness.call("terminal.status")["pid"]
    harness.stop(client)
    wait_for(lambda: not process_running(pid), description="unconfigured-provider shell cleanup")
    harness.secret = ""
    return {
        "mode": "external-without-endpoints", "tools": len(tools), "catalog_pages": pages,
        "checks": len(harness.checks) - before,
        "duration_seconds": round(time.monotonic() - started, 3),
    }


def combined_harnesses_and_commands(harness):
    started = time.monotonic()
    before = len(harness.checks)
    directory = harness.directory / "fixture-combined"
    directory.mkdir(mode=0o700)
    services = ExistingServices(harness, directory)
    installed = []
    for kind, fixture in (
        ("codex", "codex_rpc.py"), ("cc", "cc-fake.py"),
        ("pi", "pi_rpc.py"), ("opencode", "opencode_http.py"),
    ):
        state = directory / kind
        state.mkdir(mode=0o700)
        arguments = ("--fixture-dir", state) if kind in ("cc", "opencode") else ()
        if kind == "pi":
            services.start_pi(state)
        elif kind == "opencode":
            services.start_opencode(state)
        installed.append((kind, services.guard if kind in ("pi", "opencode") else FIXTURES / fixture, arguments))
    client, credentials = harness.start_client(
        "client-combined", name="combined-fixture", cwd=directory, harnesses=installed,
        extra_env=services.environment,
    )
    harness.client = client
    harness.client_id = credentials["client_id"]
    harness.oauth_login(credentials)
    tools, pages = catalog(harness, "all", "client-combined")
    services.check_no_launch()
    # The Claude fixture changes its DynamicN tool on every fresh native scan.
    # Two interleaved public scans must each keep one generation across pages.
    def finish_catalog(first):
        page, names, seen, pages = first, set(), set(), 0
        while True:
            rows = page["tools"]
            pages += 1
            if not isinstance(rows, list) or len(rows) > 64:
                raise AssertionError("all: invalid snapshot capability page")
            batch = [tool["name"] for tool in rows]
            if len(batch) != len(set(batch)) or names.intersection(batch):
                raise AssertionError("all: duplicate snapshot capability name")
            names.update(batch)
            cursor = page.get("nextCursor")
            if cursor is None:
                return names
            if not isinstance(cursor, str) or not cursor or cursor in seen or pages >= 100:
                raise AssertionError("all: invalid or repeated snapshot capability cursor")
            seen.add(cursor)
            page = harness.public_call("list", {"client_id": harness.client_id, "cursor": cursor})

    older = harness.public_call("list", {"client_id": harness.client_id})
    original_older_names = finish_catalog(older)
    old_dynamic = {name for name in original_older_names if name.startswith("claude.Dynamic")}

    def changed_catalog():
        candidate = harness.public_call("list", {"client_id": harness.client_id})
        names = {name for name in finish_catalog(candidate) if name.startswith("claude.Dynamic")}
        return candidate if names and names != old_dynamic else None

    harness.call("ssmcp.harnesses.load", {"provider": "claude", "refresh": True})
    newer = wait_for(changed_catalog, description="explicit native catalog refresh")
    # Re-read the older cursor after a new generation appeared.
    older_names = finish_catalog(older)
    newer_names = finish_catalog(newer)
    if older_names != original_older_names:
        raise AssertionError("all: the older catalog snapshot changed after refresh")
    older_dynamic = {name for name in older_names if name.startswith("claude.Dynamic")}
    newer_dynamic = {name for name in newer_names if name.startswith("claude.Dynamic")}
    harness.check(len(older_dynamic) == len(newer_dynamic) == 1
                  and older_dynamic != newer_dynamic
                  and older_names - older_dynamic == newer_names - newer_dynamic,
                  "all: interleaved catalog scans preserve their snapshots and a new scan sees native changes")
    client_id = json.dumps(harness.client_id)
    code = """
        const id = %s;
        return await Promise.all([
            call(id,'terminal.status',{}), call(id,'codex.server_diagnostics',{}),
            call(id,'pi.rpc',{type:'get_state',args:{}}), call(id,'opencode.server.info',{})
        ]);
    """ % client_id
    replies = harness.public_call("code_mode", {"code": code})["result"]
    states = [reply["structuredContent"] for reply in replies]
    harness.check(all(not reply.get("isError") for reply in replies)
                  and states[0]["running"] and states[2]["success"]
                  and states[3]["status"] == 200,
                  "all: shell and multiple discovered harness tools execute in one parallel code_mode")
    owned_pids = [states[0]["pid"], states[1]["pid"], int((directory / "cc" / "pid").read_text())]
    external_pids = [states[2]["data"]["pid"], states[3]["body"]["pid"],
                     states[3]["body"]["child_pid"]]

    commands = [
        {"command": ["/bin/sh", "-c", "sleep 0.15; printf alpha; printf alpha-err >&2"]},
        {"command": ["/bin/sh", "-c", "sleep 0.15; printf beta; exit 7"]},
        {"command": [str(directory / "missing-command")]},
    ]
    code = "return await exec.parallel(%s,%s,{concurrency:2});" % (client_id, json.dumps(commands))
    results = harness.public_call("code_mode", {"code": code})["result"]
    harness.check(results[0]["status"] == "fulfilled"
                  and results[0]["value"]["stdout"] == "alpha"
                  and results[0]["value"]["stderr"] == "alpha-err"
                  and results[0]["value"]["exit_code"] == 0
                  and results[1]["value"]["stdout"] == "beta"
                  and results[1]["value"]["exit_code"] == 7
                  and results[2]["status"] == "rejected",
                  "commands: parallel execution preserves input order, separate streams, nonzero exits and start failures")
    retained_id = results[0]["value"]["job_id"]
    code = """
        const id = %s;
        const task = await exec.start(id,{command:['/bin/sh','-c','sleep 60']});
        const pending = await exec.wait(id,task.job_id,{wait_ms:1});
        const listed = await exec.list(id);
        const retained = await exec.status(id,%s);
        const stopped = await exec.cancel(id,task.job_id);
        await exec.forget(id,task.job_id);
        const after = await exec.list(id);
        return {task,pending,listed,retained,stopped,after};
    """ % (client_id, json.dumps(retained_id))
    lifecycle = harness.public_call("code_mode", {"code": code})["result"]
    job_id = lifecycle["task"]["job_id"]
    harness.check(lifecycle["pending"]["running"]
                  and any(job["job_id"] == job_id for job in lifecycle["listed"]["jobs"])
                  and lifecycle["retained"]["exit_code"] == 0
                  and not lifecycle["stopped"]["running"]
                  and lifecycle["stopped"]["cancelled"]
                  and all(job["job_id"] != job_id for job in lifecycle["after"]["jobs"]),
                  "commands: wait preserves background work, completed results survive new starts, cancel and forget clean up")
    harness.stop(client)
    wait_for(lambda: all(not process_running(pid) for pid in owned_pids),
             description="combined owned harness and shell cleanup")
    harness.check(all(process_running(pid) for pid in external_pids),
                  "all: disconnect stops owned processes and leaves existing Pi/OpenCode services alive")
    services.close()
    wait_for(lambda: all(not process_running(pid) for pid in external_pids),
             description="test-owned external service cleanup")
    harness.check(True, "all: the fixture owner cleans up its external services and children")
    harness.secret = ""
    return {
        "mode": "combined", "tools": len(tools), "catalog_pages": pages,
        "checks": len(harness.checks) - before,
        "duration_seconds": round(time.monotonic() - started, 3),
    }


def run(harness):
    harness.prepare()
    summaries = []
    for mode in MODES:
        started = time.monotonic()
        before = len(harness.checks)
        directory = harness.directory / f"fixture-{mode}"
        directory.mkdir(mode=0o700)
        fixture = {
            "codex": "codex_rpc.py", "cc": "cc-fake.py", "pi": "pi_rpc.py",
            "opencode": "opencode_http.py",
        }.get(mode)
        backend = FIXTURES / fixture if fixture else None
        arguments = ("--fixture-dir", directory) if mode in ("cc", "opencode") else ()
        services = ExistingServices(harness, directory)
        if mode == "pi":
            services.start_pi(directory)
            backend = services.guard
        elif mode == "opencode":
            services.start_opencode(directory)
            backend = services.guard
        label = f"client-{mode}"
        client, credentials = harness.start_client(
            label, harness=mode if fixture else None, fixture=backend,
            fixture_args=arguments, name=f"{mode}-fixture", cwd=directory,
            extra_env=services.environment,
        )
        harness.client = client
        harness.client_id = credentials["client_id"]
        harness.check(credentials.get("type") == "shell"
                      and credentials["url"] == f"{harness.issuer}/{harness.user}",
                      f"{mode}: WSS registers shell and discovered tools at its authenticated account endpoint")
        harness.oauth_login(credentials)
        tools, pages = catalog(harness, mode, label)
        services.check_no_launch()
        if mode == "opencode":
            harness.check(pages >= 2, "opencode: list actually traverses more than one native capability page")
        harness.workspace_roundtrip(directory, label=mode)
        pids = CHECKERS[mode](harness, label, tools, directory)
        shell_pid = harness.call("terminal.status")["pid"]
        external_pids = pids if mode in ("pi", "opencode") else []
        owned_pids = [shell_pid] + ([] if external_pids else pids)
        harness.check(all(process_running(pid) for pid in owned_pids + external_pids),
                      f"{mode}: the shell, native provider and test jobs are alive before disconnect")
        transcript = local_log(harness, label) + (harness.directory / "account-router.log").read_text(errors="replace")
        harness.check(not any(value and value in transcript for value in harness.sensitive),
                      f"{mode}: account credentials and OAuth tokens never enter activity or daemon logs")
        old_access, old_client = harness.secret, harness.client_id
        harness.stop(client)
        harness.check(harness.rpc("ping", modern=True, secret=old_access)[0] == 200,
                      f"{mode}: device disconnect preserves independent account authorization")
        wait_for(lambda: all(item["id"] != old_client for item in harness.public_call("list")["clients"]),
                 description=f"{mode} disconnected client route removal")
        harness.check(True, f"{mode}: the disconnected client ID is no longer routable")
        status, result, _ = harness.oauth_refresh()
        harness.check(status == 200 and result.get("access_token") and result.get("refresh_token"),
                      f"{mode}: account refresh remains valid independently of device connectivity")
        harness.sensitive.extend((result["access_token"], result["refresh_token"]))
        wait_for(lambda: all(not process_running(pid) for pid in owned_pids),
                 description=f"{mode} backend and child cleanup")
        harness.check(True, f"{mode}: owned backend and child processes exit on disconnect")
        if external_pids:
            harness.check(all(process_running(pid) for pid in external_pids),
                          f"{mode}: disconnect leaves the existing native service and its children alive")
        services.close()
        wait_for(lambda: all(not process_running(pid) for pid in external_pids),
                 description=f"{mode} test-owned external service cleanup")
        harness.secret = ""
        summaries.append({
            "mode": mode, "tools": len(tools), "catalog_pages": pages,
            "checks": len(harness.checks) - before,
            "duration_seconds": round(time.monotonic() - started, 3),
        })
    summaries.append(native_only_and_cached_failure(harness))
    summaries.append(unconfigured_external_services(harness))
    summaries.append(combined_harnesses_and_commands(harness))
    return summaries


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--report", type=Path, help="write a JSON report without credentials")
    args = parser.parse_args()
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-agents-e2e-") as directory:
        harness = ServiceHarness(Path(directory), args.bin_dir)
        try:
            modes = run(harness)
        except Exception:
            harness.diagnostics()
            raise
        finally:
            harness.cleanup()
        report = {
            "status": "passed", "checks": harness.checks, "modes": modes,
            "duration_seconds": round(time.monotonic() - started, 3),
            "transport": "real account WebSocket + OAuth Authorization Code/S256 + HTTP MCP",
            "backends": "real Linux PTY; deterministic Codex, Claude Code, Pi and OpenCode protocol fixtures",
            "external_service_contract": "Pi JSONL session initialized before Unix socket publication; OpenCode HTTP server started before explicit loading; only the test owner terminates these services",
            "model_calls": False,
        }
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(f"\nPassed {len(harness.checks)} checks across {len(modes)} lazy-provider scenarios in {report['duration_seconds']}s")


if __name__ == "__main__":
    main()

