#!/usr/bin/env python3
"""Local MCP transport fixture. Never contacts a model or reads credentials."""
import json
import os
from pathlib import Path
import subprocess
import sys
import threading
import time

assert sys.argv[1:3] == ["mcp", "serve"], sys.argv
directory = Path(sys.argv[sys.argv.index("--fixture-dir") + 1])
(directory / "argv.json").write_text(json.dumps(sys.argv[1:]))
(directory / "pid").write_text(str(os.getpid()))
catalog = json.loads(Path(__file__).with_name("cc-catalog.json").read_text())
received = (directory / "received.jsonl").open("a", buffering=1)
generation = 0
pending = {}
approval_count = 0
writer_lock = threading.Lock()


def send(value):
    with writer_lock:
        print(json.dumps(value), flush=True)


def reply(identifier, result):
    send({"jsonrpc": "2.0", "id": identifier, "result": result})


def later_output(path):
    time.sleep(0.3)
    with path.open("ab") as stream:
        stream.write(b"second\n")


# Ensure a broken test cannot leave this fixture running indefinitely.
watchdog = threading.Timer(30, lambda: os._exit(124))
watchdog.daemon = True
watchdog.start()

for line in sys.stdin:
    request = json.loads(line)
    received.write(json.dumps(request) + "\n")
    identifier = request.get("id")
    if identifier in pending and ("result" in request or "error" in request):
        call_id = pending.pop(identifier)
        reply(call_id, {
            "content": [{"type": "text", "text": "approval settled"}],
            "structuredContent": {"approval": request},
        })
        continue
    method = request.get("method")
    parameters = request.get("params", {})
    if method == "initialize":
        reply(identifier, {
            "protocolVersion": parameters["protocolVersion"],
            "capabilities": {"tools": {"listChanged": True}},
            "serverInfo": {"name": "ssmcp-cc-fixture", "version": "1"},
        })
    elif method == "notifications/initialized":
        pass
    elif method == "tools/list":
        if "cursor" not in parameters:
            generation += 1
            reply(identifier, {"tools": catalog[:1], "nextCursor": "page-two"})
        elif "--repeat-cursor" in sys.argv:
            reply(identifier, {"tools": [], "nextCursor": "page-two"})
        else:
            assert parameters["cursor"] == "page-two"
            reply(identifier, {"tools": catalog[1:] + [{
                "name": "Dynamic" + str(generation),
                "inputSchema": {"type": "object"},
            }]})
    elif method == "tools/call":
        name = parameters["name"]
        arguments = parameters.get("arguments", {})
        if name == "NeedsApproval":
            approval_count += 1
            approval_id = "approval-" + str(approval_count)
            pending[approval_id] = identifier
            send({
                "jsonrpc": "2.0", "id": approval_id, "method": "elicitation/create",
                "params": {"message": "Allow fixture action?", "requestedSchema": {
                    "type": "object", "properties": {"allowed": {"type": "boolean"}}
                }},
            })
        elif name == "NativeFutureTool":
            if arguments.get("spawn_child"):
                child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(60)"])
                (directory / "child-pid").write_text(str(child.pid))
            send({"jsonrpc": "2.0", "method": "notifications/progress", "params": {
                "progressToken": parameters["_meta"]["progressToken"], "progress": 1,
                "message": "fixture progress",
            }})
            reply(identifier, {
                "content": [
                    {"type": "text", "text": "native result"},
                    {"type": "image", "mimeType": "image/png", "data": "AAEC"},
                    {"type": "resource", "resource": {
                        "uri": "fixture://native-result", "mimeType": "text/plain", "text": "embedded"
                    }},
                ],
                "structuredContent": {"future": {"arguments": arguments}},
                "isError": arguments.get("error", False),
                "_meta": {"vendor/result": [1, 2, 3]},
                "futureResultField": {"keep": "this too"},
            })
            send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
        elif name == "Bash":
            path = directory / "background.output"
            if arguments.get("fixture_mode") == "symlink":
                target = directory / "not-an-output-file"
                target.write_text("DO_NOT_TAIL\n")
                path = directory / "background.link"
                path.symlink_to(target)
            else:
                path.write_bytes(b"first\n")
                path.chmod(0o600)
                threading.Thread(target=later_output, args=(path,), daemon=True).start()
            reply(identifier, {"content": [{"type": "text", "text": json.dumps({
                "stdout": "", "stderr": "", "backgroundTaskId": "fixture-job",
                "backgroundOutputPath": str(path),
            })}]})
        else:
            reply(identifier, {"content": [{"type": "text", "text": name}],
                               "structuredContent": {"native_name": name, "arguments": arguments}})
    elif identifier is not None:
        send({"jsonrpc": "2.0", "id": identifier,
              "error": {"code": -32601, "message": "fixture method not found"}})
