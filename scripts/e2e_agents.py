#!/usr/bin/env python3
"""Exercise the native PTY and exact tool catalog through WSS, OAuth and MCP."""

import argparse
import json
from pathlib import Path
import tempfile
import time

from e2e import BUILTIN_TOOLS, SHELL_TOOLS, Harness, process_running, wait_for


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


def native_defaults(harness, label, context):
    rows = harness.call("ssmcp.harnesses.list")["harnesses"]
    names = {"ssmcp"}
    providers = {row["provider"]: row for row in rows}
    harness.check(len(rows) == len(providers) == len(names) and set(providers) == names,
                  f"{context}: only the explicit SSMCP peer has lazy state")
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
    for removed in ("codex", "claude", "pi", "opencode"):
        full_call(harness, "ssmcp.harnesses.load", {"provider": removed}, expect_error=True)
    harness.check(harness.external_invocations(label) == [],
                  "removed providers reject loading without launching processes")
    return providers


def catalog(harness, mode, label):
    native_defaults(harness, label, mode)
    status, value, _ = harness.rpc("tools/list", modern=True)
    harness.check(status == 200 and [tool["name"] for tool in value["result"]["tools"]] == ["link", "list", "code_mode", "collab", "await", "messages"],
                  f"{mode}: public MCP catalog contains exactly link, list, code_mode, collab, await and messages")
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
    harness.check(set(names) == BUILTIN_TOOLS | SHELL_TOOLS,
                  "native catalog exposes the exact shell and built-in capabilities")
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
    return tools, pages


def local_log(harness, label):
    return (harness.directory / f"{label}.log").read_text(errors="replace")


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


def run(harness):
    harness.prepare()
    summaries = []
    for mode in ("shell",):
        started = time.monotonic()
        before = len(harness.checks)
        directory = harness.directory / f"fixture-{mode}"
        directory.mkdir(mode=0o700)
        label = "client-shell"
        client, credentials = harness.start_client(label, name="shell-fixture", cwd=directory)
        harness.client = client
        harness.client_id = credentials["client_id"]
        harness.check(credentials.get("type") == "shell"
                      and credentials["url"] == f"{harness.issuer}/{harness.user}",
                      f"{mode}: WSS registers shell and discovered tools at its authenticated account endpoint")
        harness.oauth_login(credentials)
        tools, pages = catalog(harness, mode, label)
        harness.workspace_roundtrip(directory, label=mode)
        owned_pids = check_shell(harness, label, tools, directory)
        harness.check(all(process_running(pid) for pid in owned_pids),
                      "the native shell and its child are alive before disconnect")
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
            "transport": "real account WebSocket + OAuth Authorization Code/S256 + HTTP MCP",
            "backends": "real Linux PTY",
            "model_calls": False,
        }
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(f"\nPassed {len(harness.checks)} checks across {len(modes)} lazy-provider scenarios in {report['duration_seconds']}s")


if __name__ == "__main__":
    main()
