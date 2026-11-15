#!/usr/bin/env python3
"""Versioned Codex App Server fixture. No network, model or existing credentials."""
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


def reply(request, result):
    emit({"jsonrpc": "2.0", "id": request["id"], "result": result})


for line in sys.stdin.buffer:
    request = json.loads(line)
    method = request.get("method")
    if method == "initialize":
        assert request["params"]["capabilities"]["experimentalApi"]
        reply(request, {"userAgent": "ssmcp-fixture"})
    elif method == "initialized":
        initialized = True
    elif method is None:
        assert request["id"] == "approval-1"
        answers.append(request)
        emit({"method": "serverRequest/resolved", "params": {"requestId": "approval-1", "threadId": "thread-1"}})
        emit({"method": "turn/completed", "params": {"turn": {"id": "turn-1", "status": "completed"}, "decision": request.get("result")}})
    else:
        assert initialized
        if method == "server/diagnostics":
            reply(request, {"pid": os.getpid(), "answers": answers})
        elif method == "configRequirements/read":
            assert request["params"] is None, "unit params must remain null"
            reply(request, {"requirements": None})
        elif method == "fixture/cancel":
            emit({"method": "notifications/cancelled", "params": {"requestId": "approval-1"}})
            reply(request, {})
        elif method == "thread/start":
            reply(request, {"thread": {"id": "thread-1"}, "params": request["params"]})
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
