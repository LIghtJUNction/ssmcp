#!/usr/bin/env python3
"""Exercise the protocol-only module actor with a real account relay and local attach gateway."""
import argparse
import json
import os
from pathlib import Path
import secrets
import subprocess
import tempfile
import time
import urllib.request

from e2e import Harness


def until(check, proc=None):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        value = check()
        if value:
            return value
        if proc is not None and proc.poll() is not None:
            raise AssertionError(f"owned chat process exited ({proc.returncode})")
        time.sleep(0.05)
    raise AssertionError(f"condition timed out at {check.__code__.co_firstlineno}")


def exercise(client, fixture, root):
    h = Harness(root, client.parent)
    h.fixture_binary = fixture
    peers = []
    try:
        h.prepare()
        registry = root / "sessions"
        registry.mkdir(mode=0o700)
        base = {key: value for key, value in os.environ.items() if not key.startswith("SSMCP_")}
        destination = f"{h.identity['username']}@{h.origin}"
        for index in range(2):
            directory = root / f"owner-{index}"
            directory.mkdir(mode=0o700)
            env = {**base, "HOME": str(directory), "SSMCP_SESSION_HOME": str(registry),
                   "SSMCP_AUTH_HOME": str(directory / "auth")}
            instance = secrets.token_hex(16)
            log = (directory / "chat.log").open("w")
            try:
                proc = subprocess.Popen([client, "__magisk-chat-frontend", destination, instance],
                                        stdin=subprocess.PIPE, stdout=log, stderr=log, cwd=directory,
                                        env=env, start_new_session=True)
            finally:
                log.close()
            peers.append((proc, "live:" + instance))
            until((registry / "live" / instance / "overview.json").is_file, proc)

        env = {**base, "HOME": str(root), "SSMCP_SESSION_HOME": str(registry)}
        pairing = root / "pairing.json"
        proc = h.spawn("attach-gateway", [client, "gui", "serve", "--session-mode", "attach",
                                         "--pairing-file", str(pairing)], env=env, cwd=root)
        until(pairing.is_file, proc)
        pair = json.loads(pairing.read_text())

        def http(path, payload):
            request = urllib.request.Request(pair["url"] + "/api/gui/v1/" + path,
                data=json.dumps(payload).encode(), headers={"Origin": pair["url"],
                "Authorization": "Bearer " + pair["token"], "Content-Type": "application/json"})
            with urllib.request.urlopen(request, timeout=10) as reply:
                return json.load(reply)

        def bridge(connection, op, **kwargs):
            return http(f"connections/{connection}/bridge", {"version": 1, "id": secrets.token_hex(16),
                "operation": {"op": op, **kwargs}})

        def state(connection, generation, resource):
            response = bridge(connection, "state", resource=resource, generation=generation, query={})
            assert response["body"]["kind"] == "state", response["body"]
            return response["body"]["value"]["value"]

        active = []
        for _, target in peers:
            opening = http("connections", {"request_id": secrets.token_hex(16), "session": target, "read_only": False})
            connection = opening["connection_id"]
            caps = bridge(connection, "capabilities")
            generation = caps["generation"]
            assert {cmd["id"] for cmd in caps["body"]["value"]["commands"]} == {"chat.send"}
            assert {res["id"] for res in caps["body"]["value"]["resources"]} == {"runtime", "conversation", "threads", "chat.queue"}
            client_id = state(connection, generation, "runtime")["client_id"]
            until(lambda: state(connection, generation, "threads")["can_send"] is True)
            active.append((connection, generation, client_id))
        left, right = active
        assert left[2] != right[2]
        listed = h.request("GET", path="/api/client/sessions", secret=h.account_token)
        assert listed[0] == 200 and {c["id"] for c in listed[1]["clients"]} == {left[2], right[2]}
        h.oauth_login({"client_id": left[2]})
        names = {tool["name"] for tool in h.native_catalog(left[2])}
        assert names == {"ssmcp.ui.get", "ssmcp.ui.message", "ssmcp.ui.reply", "ssmcp.ui.wait"}, names

        def remote(name, arguments):
            return h.request("POST", path="/api/client/call", secret=h.account_token, body={
                "client_id": left[2], "name": name, "arguments": arguments})[:2]

        status, identity = remote("ssmcp.ui.get", {"path": "/identity"})
        assert status == 200 and identity["structuredContent"]["value"]["client_id"] == left[2], (status, identity)
        for tool, args in [("terminal.write", {"data":"unavailable"}), ("ssmcp.exec.start", {"command":["/bin/false"]}),
                           ("ssmcp.fs.read_text", {"path":"private.txt"}), ("ssmcp.client.reload", {}),
                           ("ssmcp.ui.get", {"path":"/config"})]:
            assert remote(tool, args)[0] == 502, tool
        for command in ["exec.start", "session.new", "session.cd", "terminal.write", "client.update"]:
            denied = bridge(left[0], "command", generation=left[1], command=command, arguments={})
            assert denied["body"]["kind"] == "error", command

        secret = "MUST_STAY_IN_WORKSPACE_" + secrets.token_hex(12)
        (root / "owner-0" / "private.txt").write_text(secret)
        message = "Please keep @private.txt and /new as literal text."
        receipt = bridge(left[0], "command", generation=left[1], command="chat.send",
                         arguments={"request_id": secrets.token_hex(16), "target":"next", "text":message})
        queued = receipt["body"]["value"]["value"]
        assert queued["client_id"] == left[2]
        until(lambda: any(item["id"] == queued["message_id"] and item["text"] == message
                          and item["status"] == "queued" for item in state(left[0], left[1], "chat.queue")["items"]))
        assert not state(right[0], right[1], "chat.queue")["items"]
        answer = "confirmed independent chat " + secrets.token_hex(8)
        status, published = remote("ssmcp.ui.message", {"kind": "answer", "text": answer})
        assert status == 200 and not published.get("isError"), (status, published)
        until(lambda: answer in json.dumps(state(left[0], left[1], "conversation")))
        assert answer not in json.dumps(state(right[0], right[1], "conversation"))
        print("PASS two actual relay identities, four native tools, authenticated round trip, rejected device operations")
    finally:
        for proc, _ in peers:
            proc.stdin.close()
            try:
                proc.wait(timeout=7)
            finally:
                if proc.poll() is None:
                    h.stop(proc)
        h.cleanup()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="ssmcp-magisk-chat-") as root:
        exercise(args.client.resolve(strict=True), args.fixture_binary.resolve(strict=True), Path(root))
