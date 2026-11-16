#!/usr/bin/env python3
"""Versioned Codex App Server fixture. No network, model or existing credentials."""
import copy
import json
import os
from pathlib import Path
import sys


def emit(value):
    print(json.dumps(value, ensure_ascii=False, separators=(",", ":")), flush=True)


methods = {
    "initialize": {"$ref": "#/definitions/Params"},
    "thread/start": {"$ref": "#/definitions/Params"},
    "turn/start": {"$ref": "#/definitions/Params"},
    "command/exec": {"$ref": "#/definitions/Params"},
    "command/exec/terminate": {"$ref": "#/definitions/Params"},
    "server/diagnostics": {"$ref": "#/definitions/Params"},
    "configRequirements/read": {"type": "null"},
    "mcpServerStatus/list": {"$ref": "#/definitions/Params"},
    "mcpServer/tool/call": {"$ref": "#/definitions/Params"},
}
assert sys.argv[1] == "app-server", sys.argv
if "generate-json-schema" in sys.argv:
    directory = Path(sys.argv[sys.argv.index("--out") + 1])
    schema = {
        "oneOf": [{"properties": {"method": {"enum": [method]}, "params": params}} for method, params in methods.items()],
        "definitions": {"Params": {"type": "object", "properties": {"recursive": {"$ref": "#/definitions/Node"}}},
                        "Node": {"type": "object", "properties": {"node": {"$ref": "#/definitions/Node"}}}},
    }
    (directory / "ClientRequest.json").write_text(json.dumps(schema))
    (directory / "ApprovalResponse.json").write_text(json.dumps({"type": "object", "properties": {"decision": {"enum": ["accept", "decline"]}}}))
    sys.exit(0)

assert sys.argv[2:4] == ["--listen", "stdio://"], sys.argv
initialized = False
answers = []
held = None
held_mcp = None
held_pages = []
mcp_mode = "healthy"
requests = []
thread_starts = 0
mcp_calls = []


def reply(request, result):
    emit({"jsonrpc": "2.0", "id": request["id"], "result": result})


def native_tool(name, title, annotations=True):
    tool = {
        "name": name,
        "title": title,
        "description": "Native fixture tool; preserve every field.",
        "inputSchema": {
            "type": "object",
            "$defs": {"Path": {"type": "string", "minLength": 1}},
            "properties": {"path": {"$ref": "#/$defs/Path"}, "fail": {"type": "boolean"},
                           "choice": {"oneOf": [{"type": "string"}, {"type": "integer"}]}},
            "additionalProperties": True,
        },
        "outputSchema": {"type": "object", "properties": {"arguments": {"type": "object"}}},
        "_meta": {"fixture/native": {"keep": [1, {"nested": True}]}},
        "x-native-extension": {"value": "unchanged"},
    }
    if annotations:
        tool["annotations"] = {"readOnlyHint": True, "idempotentHint": True, "customHint": "native"}
    return tool


def servers():
    first = [
        native_tool("read/file", "native-read"),
        native_tool("read_file", "native-underscore"),
        native_tool("read", "native-dot-server"),
        native_tool("approval", "native-approval"),
        native_tool("读取/文件", "native-unicode"),
        native_tool("long" * 80, "native-long"),
    ]
    if mcp_mode == "updated":
        first = [tool for tool in first if tool["title"] != "native-underscore"]
        first[0]["inputSchema"]["properties"]["revision"] = {"const": 2}
        first.append(native_tool("new-tool", "native-new"))
    result = [
        {"name": "code.graph", "tools": {tool["name"]: tool for tool in first}},
        {"name": "code", "tools": {"graph.read": native_tool("graph.read", "native-boundary")}},
        {"name": "other", "tools": {"echo": native_tool("echo", "native-no-annotations", False),
                                    "missing-schema": {"name": "missing-schema", "description": "No schema supplied"}}},
    ]
    if mcp_mode == "reordered":
        result.reverse()
    if mcp_mode == "down":
        return [{"name": server["name"], "tools": None} for server in result]
    return result


def tool_result(params):
    return {
        "content": [{"type": "text", "text": "native-result"},
                    {"type": "image", "mimeType": "image/png", "data": "aW1hZ2U="}],
        "isError": params.get("arguments", {}).get("fail", False),
        "structuredContent": copy.deepcopy(params),
        "_meta": {"fixture/result": "preserved"},
        "x-native-result": {"value": 42},
    }


for line in sys.stdin.buffer:
    request = json.loads(line)
    method = request.get("method")
    if method:
        requests.append(method)
    if method == "initialize":
        assert request["params"]["capabilities"]["experimentalApi"]
        reply(request, {"userAgent": "ssmcp-fixture"})
    elif method == "initialized":
        initialized = True
    elif method is None:
        answers.append(request)
        if request["id"] == "mcp-approval-1":
            assert held_mcp is not None
            result = tool_result(held_mcp["params"])
            result["isError"] = request.get("result", {}).get("action") != "accept"
            result["structuredContent"]["approval"] = request.get("result", request.get("error"))
            emit({"method": "serverRequest/resolved", "params": {"requestId": "mcp-approval-1"}})
            reply(held_mcp, result)
            held_mcp = None
        else:
            assert request["id"] == "approval-1"
            emit({"method": "serverRequest/resolved", "params": {"requestId": "approval-1", "threadId": "thread-1"}})
            emit({"method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}, "decision": request.get("result")}})
    else:
        assert initialized
        if method == "server/diagnostics":
            reply(request, {"pid": os.getpid(), "answers": answers, "methods": requests,
                            "thread_starts": thread_starts, "mcp_calls": mcp_calls,
                            "model_requests": sum(name == "turn/start" for name in requests)})
        elif method == "configRequirements/read":
            assert request["params"] is None, "unit params must remain null"
            reply(request, {"requirements": None})
        elif method == "fixture/cancel":
            emit({"method": "notifications/cancelled", "params": {"requestId": "approval-1"}})
            reply(request, {})
        elif method == "fixture/mcp_mode":
            mcp_mode = request["params"]["mode"]
            if mcp_mode != "timeout":
                for held_page in held_pages:
                    reply(held_page, {"data": servers()[2:], "nextCursor": None})
                held_pages.clear()
            emit({"method": "notifications/tools/list_changed", "params": {}})
            reply(request, {})
        elif method == "fixture/mcp_specs":
            reply(request, {"servers": servers()})
        elif method == "fixture/disconnect":
            reply(request, {})
            sys.exit(0)
        elif method == "thread/start":
            thread_starts += 1
            reply(request, {"thread": {"id": "thread-1"}, "params": request["params"]})
        elif method == "mcpServerStatus/list":
            params = request["params"]
            assert params["detail"] == "toolsAndAuthOnly"
            assert 1 <= params["limit"] <= 100
            cursor = params.get("cursor")
            if mcp_mode == "malformed":
                reply(request, {"data": "not-a-server-list"})
            elif mcp_mode == "invalid_cursor":
                reply(request, {"data": [], "nextCursor": 3})
            elif mcp_mode == "loop":
                reply(request, {"data": [], "nextCursor": "A" if cursor in (None, "B") else "B"})
            elif mcp_mode == "endless":
                reply(request, {"data": [], "nextCursor": str(int(cursor or 0) + 1)})
            elif cursor is None:
                reply(request, {"data": servers()[:2], "nextCursor": "page-2"})
            elif cursor == "page-2" and mcp_mode == "page_error":
                emit({"jsonrpc": "2.0", "id": request["id"], "error": {"code": -32001, "message": "MCP page unavailable", "data": {"server": "other"}}})
            elif cursor == "page-2" and mcp_mode == "timeout":
                # Keep the peer responsive while leaving this one request open.
                held_pages.append(request)
                emit({"method": "fixture/mcpPageWaiting", "params": {}})
            else:
                assert cursor == "page-2"
                reply(request, {"data": servers()[2:], "nextCursor": None})
        elif method == "mcpServer/tool/call":
            params = request["params"]
            assert params["threadId"] == "thread-1"
            assert any(server["name"] == params["server"] and isinstance(server["tools"], dict)
                       and params["tool"] in server["tools"] for server in servers()), params
            mcp_calls.append(copy.deepcopy(params))
            if params.get("arguments", {}).get("nativeRpcError"):
                emit({"jsonrpc": "2.0", "id": request["id"], "error": {
                    "code": -32042, "message": "Fixture native routing failed",
                    "data": {"server": params["server"], "preserved": [1, {"opaque": True}]}}})
            elif params["tool"] == "approval":
                held_mcp = request
                emit({"jsonrpc": "2.0", "id": "mcp-approval-1", "method": "mcpServer/elicitation/request",
                      "params": {"serverName": params["server"], "message": "Fixture approval",
                                 "requestedSchema": {"type": "object", "properties": {}}}})
            else:
                reply(request, tool_result(params))
        elif method == "turn/start":
            reply(request, {"turn": {"id": "turn-1", "status": "inProgress"}})
            emit({"method": "item/commandExecution/outputDelta", "params": {"delta": "live-before-approval\n"}})
            emit({"id": "approval-1", "method": "item/commandExecution/requestApproval", "params": {"command": "fixture-only", "threadId": "thread-1", "turnId": "turn-1"}})
        elif method == "command/exec":
            if request["params"].get("command") == ["hold"]:
                held = request
                emit({"method": "command/exec/outputDelta", "params": {"processId": "held", "deltaBase64": "bGl2ZQ=="}})
            else:
                reply(request, {"exitCode": 0, "stdout": "fixture-output", "stderr": "", "params": request["params"]})
        elif method == "command/exec/terminate":
            reply(request, {})
            if held:
                reply(held, {"exitCode": 137, "stdout": "", "stderr": ""})
                held = None
        else:
            reply(request, {"native_method": method, "params": request["params"]})
