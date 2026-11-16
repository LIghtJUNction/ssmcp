#!/usr/bin/env python3
"""Exercise all five modes through real local SSH, HTTP MCP and OAuth.

Shell uses a real PTY. Agent modes use deterministic local protocol fixtures;
no installed agent, model account, existing SSH identity or paid API is used.
Run after cargo build --locked, on Linux with an OpenSSH server available.
"""

from __future__ import annotations

import argparse
import base64
import concurrent.futures
import json
import re
from pathlib import Path
import tempfile
import time

from e2e import BUILTIN_TOOLS, SHELL_TOOLS, Harness, PUBLIC_URL, process_running, wait_for


ROOT = Path(__file__).resolve().parents[1]
FIXTURES = ROOT / "tests" / "fixtures"
MODES = ("shell", "codex", "cc", "pi", "opencode")


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


def catalog(harness, mode):
    status, value, _ = harness.rpc("tools/list", modern=True)
    harness.check(status == 200 and [tool["name"] for tool in value["result"]["tools"]] == ["list", "code_mode"],
                  f"{mode}: public MCP catalog contains exactly list and code_mode")
    inventory = harness.public_call("list")
    harness.check(len(inventory["clients"]) == 1
                  and inventory["clients"][0]["id"] == harness.client_id
                  and inventory["clients"][0]["type"] == mode,
                  f"{mode}: list discovers the selected client and backend type")
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
    prefixes = ("terminal.",) if mode == "shell" else (f"{mode}.", f"ssmcp.{mode}.")
    shared = {name for name in names if name.startswith(("ssmcp.fs.", "ssmcp.net.", "ssmcp.rsync.", "ssmcp.context.", "ssmcp.skills.", "ssmcp.agents."))}
    harness.check(shared == BUILTIN_TOOLS,
                  f"{mode}: exact universal workspace, context and rsync capabilities are discoverable")
    native = set(names) - BUILTIN_TOOLS
    harness.check(bool(native) and all(name.startswith(prefixes) for name in native),
                  f"{mode}: remaining native capabilities belong to the selected backend")
    harness.check(all(isinstance(tool.get("inputSchema"), dict) for tool in tools),
                  f"{mode}: all discovered tools retain JSON schemas")
    other_tool = "pi.get_state" if mode == "shell" else "terminal.status"
    full_call(harness, other_tool, expect_error=True)
    harness.check(True, f"{mode}: a tool from another mode is rejected")
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
    expected = {**native, "name": "cc." + native["name"]}
    harness.check(next(tool for tool in tools if tool["name"] == expected["name"]) == expected,
                  "cc: native schema, annotations and vendor catalog fields are unchanged")
    arguments = {"nested": [3, 5, 8], "spawn_child": True}
    result = full_call(harness, "cc.NativeFutureTool", arguments)
    harness.check(result == {
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
    }, "cc: full native MCP content, image, resource, metadata and future fields survive SSH")
    error = full_call(harness, "cc.NativeFutureTool", {"error": True}, expect_error=True)
    harness.check(error["structuredContent"]["future"]["arguments"] == {"error": True},
                  "cc: native tool errors retain their structured result")
    wait_for(lambda: "fixture progress" in local_log(harness, label),
             description="Claude native progress on local stderr")
    harness.check(True, "cc: native progress appears on local stderr before event polling")
    full_call(harness, "cc.Bash", {"fixture_mode": "background"})
    wait_for(lambda: log_has_line(harness, label, "first")
             and log_has_line(harness, label, "second"),
             description="Claude background output on local stderr")
    events, _ = events_until(harness, "ssmcp.cc.events", lambda values: b"second\n" in b"".join(
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
        running = pool.submit(full_call, harness, "cc.NeedsApproval")
        request = pending_request(harness, "ssmcp.cc.requests")
        received = [json.loads(line) for line in (directory / "received.jsonl").read_text().splitlines()]
        harness.check(not running.done() and not any(
            item.get("id") == request["id"] and ("result" in item or "error" in item) for item in received
        ), "cc: native elicitation is never automatically approved")
        harness.call("ssmcp.cc.respond", {"id": request["id"], "result": {"action": "decline"}})
        reply = running.result(timeout=5)
        harness.check(reply["structuredContent"]["approval"]["result"] == {"action": "decline"},
                      "cc: an explicit elicitation response reaches the native process")
    return [int((directory / name).read_text()) for name in ("pid", "child-pid")]


def check_pi(harness, label, tools, _directory):
    harness.check(len(tools) == 37 + len(BUILTIN_TOOLS),
                  "pi: all 33 native commands, four bridge helpers and shared builtins are discoverable")
    state = harness.call("pi.get_state")
    harness.check(state["success"] and state["command"] == "get_state", "pi: full native response envelope is preserved")
    future = harness.call("pi.rpc", {"type": "future_control", "args": {"future": {"nested": [2, 7]}}})
    harness.check(future["data"] == {"future": {"nested": [2, 7]}}, "pi: generic RPC preserves future native fields")
    error = full_call(harness, "pi.rpc", {"type": "future_failure", "args": {}}, expect_error=True)
    harness.check(error["structuredContent"]["success"] is False
                  and error["structuredContent"]["error"] == "native fixture rejection",
                  "pi: native failures retain their envelope and MCP error flag")
    accepted = harness.call("pi.prompt", {"message": "fixture prompt"})
    harness.check(accepted["data"]["disposition"] == "started", "pi: prompt acceptance is returned separately from completion")
    wait_for(lambda: log_has_line(harness, label, "agent_settled"),
             description="Pi settled event on local stderr")
    harness.check(True, "pi: native completion reaches local stderr before event polling")
    events_until(harness, "ssmcp.pi.events", lambda events: any(
        event.get("type") == "agent_settled" for event in events
    ))
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        running = pool.submit(harness.call, "pi.prompt", {"message": "/dialog confirm"})
        request = pending_request(harness, "ssmcp.pi.requests")
        harness.check(not running.done() and harness.call("pi.get_state")["data"]["answers"] == 0,
                      "pi: extension UI waits for an explicit answer")
        harness.call("ssmcp.pi.respond", {"id": request["id"], "confirmed": False})
        reply = running.result(timeout=5)
        harness.check(reply["data"]["answer"]["confirmed"] is False,
                      "pi: explicit extension-UI rejection reaches the native process")
    with concurrent.futures.ThreadPoolExecutor(max_workers=1) as pool:
        running = pool.submit(harness.call, "pi.bash", {"command": "fixture-held-command"})
        try:
            wait_for(lambda: log_has_line(harness, label, "first"),
                     description="Pi bash delta on local stderr")
            harness.check(not running.done(), "pi: Bash output is visible before the native command returns")
        finally:
            harness.call("pi.abort_bash")
        reply = running.result(timeout=5)
        harness.check(reply["data"]["cancelled"] is True and reply["data"]["exitCode"] == 130,
                      "pi: abort remains usable while a native command is pending")
    return [state["data"]["pid"]]


def check_opencode(harness, label, tools, directory):
    names = {tool["name"] for tool in tools}
    harness.check(len(tools) == 78 + len(BUILTIN_TOOLS) and "opencode.fixture.extra69" in names,
                  "opencode: all native OpenAPI operations survive multi-page MCP discovery")
    info = harness.call("opencode.server.info")
    process = info["body"]["data"]
    harness.check(info["status"] == 200 and process["authenticated"]
                  and info["headers"]["x-fixture-result"] == "preserved",
                  "opencode: private authenticated HTTP responses retain status and headers")
    schema = harness.call("opencode.schema", {"operation_id": "fixture.echo"})
    harness.check(schema["operation"]["x-fixture-extension"] == {"keep": [1, 2, 3]}
                  and "EchoInput" in schema["components"]["schemas"],
                  "opencode: raw native operation extensions and referenced schemas are available")
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
    requests = [json.loads(line) for line in (directory / "requests.jsonl").read_text().splitlines()]
    harness.check(all(request["authenticated"] for request in requests),
                  "opencode: schema, event and API requests all use the private Basic credential")
    return [process["pid"], process["child_pid"]]


CHECKERS = {
    "shell": check_shell, "codex": check_codex, "cc": check_cc,
    "pi": check_pi, "opencode": check_opencode,
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
        label = f"client-{mode}"
        client, credentials = harness.start_client(label, mode=mode, backend=backend,
                                                   backend_args=arguments, name=f"{mode}-fixture", cwd=directory)
        harness.client = client
        harness.client_id = credentials["client_id"]
        harness.check(credentials.get("type") == mode
                      and credentials["url"] == f"{PUBLIC_URL}/{harness.user}",
                      f"{mode}: SSH registers the selected backend at its authenticated user path")
        harness.oauth_login(credentials)
        tools, pages = catalog(harness, mode)
        if mode == "opencode":
            harness.check(pages >= 2, "opencode: list actually traverses more than one native capability page")
        harness.workspace_roundtrip(directory, label=mode)
        pids = CHECKERS[mode](harness, label, tools, directory)
        harness.check(all(process_running(pid) for pid in pids), f"{mode}: owned backend and test jobs are alive before disconnect")
        transcript = local_log(harness, label) + (harness.directory / "daemon.log").read_text(errors="replace")
        harness.check(not any(value and value in transcript for value in harness.sensitive),
                      f"{mode}: login codes and OAuth tokens never enter activity or daemon logs")
        old_access = harness.secret
        harness.stop(client)
        wait_for(lambda: harness.request(body={}, secret=old_access)[0] == 401,
                 description=f"{mode} OAuth access revocation")
        harness.check(True, f"{mode}: disconnect immediately revokes the access token")
        status, result, _ = harness.oauth_refresh()
        harness.check(status == 400 and result.get("error") == "invalid_grant",
                      f"{mode}: disconnect also prevents refresh-token renewal")
        wait_for(lambda: all(not process_running(pid) for pid in pids),
                 description=f"{mode} backend and child cleanup")
        harness.check(True, f"{mode}: owned backend and child processes exit on disconnect")
        harness.secret = ""
        summaries.append({
            "mode": mode, "tools": len(tools), "catalog_pages": pages,
            "checks": len(harness.checks) - before,
            "duration_seconds": round(time.monotonic() - started, 3),
        })
    return summaries


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--report", type=Path, help="write a JSON report without credentials")
    args = parser.parse_args()
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-agents-e2e-") as directory:
        harness = Harness(Path(directory), args.bin_dir)
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
            "transport": "real OpenSSH public-key + OAuth Authorization Code/S256 + HTTP MCP",
            "backends": "real Linux PTY; deterministic Codex, Claude Code, Pi and OpenCode protocol fixtures",
            "model_calls": False,
        }
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(f"\nPassed {len(harness.checks)} checks across {len(modes)} modes in {report['duration_seconds']}s")


if __name__ == "__main__":
    main()
