#!/usr/bin/env python3
"""Deterministic Pi JSONL fixture; never launches a model or reads credentials."""

import json
import os
import sys


assert sys.argv[1:3] == ["--mode", "rpc"], sys.argv
calls = 0
answers = 0
held = None
dialogs = {}


def emit(value):
    sys.stdout.write(json.dumps(value, ensure_ascii=False, separators=(",", ":")) + "\n")
    sys.stdout.flush()


def response(command, data=None, *, error=None):
    value = {
        "type": "response",
        "id": command["id"],
        "command": command["type"],
        "success": error is None,
    }
    if error is not None:
        value["error"] = error
    elif data is not None:
        value["data"] = data
    emit(value)


for line in sys.stdin.buffer:
    command = json.loads(line)
    kind = command["type"]
    if kind == "extension_ui_response":
        answers += 1
        pending = dialogs.pop(command["id"])
        emit({"type": "fixture_dialog_answer", "answer": command})
        response(pending, {"disposition": "handled", "answer": command})
        continue

    calls += 1
    if kind == "get_state":
        response(command, {"pid": os.getpid(), "calls": calls, "answers": answers})
    elif kind == "bash":
        held = command
        emit({"type": "bash_execution_update", "id": command["id"], "delta": "first\nsecond\u2028line\u2029\n"})
    elif kind == "abort_bash":
        # The younger command deliberately completes before the older one.
        response(command)
        if held is not None:
            response(held, {"output": "aborted", "exitCode": 130, "cancelled": True, "truncated": False})
            held = None
    elif kind == "prompt":
        method = command.get("message", "").removeprefix("/dialog ")
        if method in ("confirm", "select", "input", "editor"):
            request_id = "dialog-" + command["id"]
            dialogs[request_id] = command
            emit({"type": "extension_ui_request", "id": "notice-" + command["id"], "method": "notify", "message": "Waiting for an explicit answer"})
            request = {"type": "extension_ui_request", "id": request_id, "method": method, "title": "Fixture dialog"}
            if method == "confirm":
                request["message"] = "Allow this test?"
            elif method == "select":
                request["options"] = ["allow", "deny"]
            emit(request)
        else:
            response(command, {"disposition": "started"})
            emit({"type": "agent_end", "messages": [], "willRetry": True})
            emit({"type": "agent_settled", "aborted": False})
    elif kind == "get_commands":
        response(command, {"commands": [{"name": "fixture-command", "source": "extension", "sourceInfo": {"path": __file__}}]})
    elif kind == "future_control":
        response(command, {key: value for key, value in command.items() if key not in ("id", "type")})
    elif kind == "future_failure":
        response(command, error="native fixture rejection")
    elif kind == "new_session":
        response(command, {"cancelled": True})
    else:
        response(command, error="Unknown command: " + kind)
