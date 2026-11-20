#!/usr/bin/env python3
"""Real authenticated MCP and private Unix programs share one listener receipt.

Uses the existing isolated account router, synthetic upstream GitHub, real CLI
and owned PTY. No model, installed login, production server or host input.
"""
import argparse
import hashlib
import http.client
import json
import os
import signal
from pathlib import Path
import select
import socket
import stat
import struct
import subprocess
import sys
import tempfile
import time

from e2e import MODERN, PROTOCOL_META, CAPABILITIES_META
from e2e_force_send import ForceSend
from e2e_composer import HOME, DOWN, ENTER
from e2e_dashboard import text


def exact(stream, length):
    data = bytearray()
    while len(data) < length:
        chunk = stream.recv(length - len(data))
        if not chunk:
            raise RuntimeError("owned listener socket ended mid-frame")
        data.extend(chunk)
    return bytes(data)


def worker(mode):
    config = json.loads(sys.stdin.readline())
    request_id = 0
    for line in sys.stdin:
        command = json.loads(line)
        if command.get("stop"):
            break
        try:
            arguments = command["arguments"]
            if mode == "unix":
                with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stream:
                    stream.settimeout(10)
                    stream.connect(config["endpoint"])
                    payload = json.dumps(arguments, ensure_ascii=False).encode()
                    stream.sendall(struct.pack(">I", len(payload)) + payload)
                    length, = struct.unpack(">I", exact(stream, 4))
                    if length > 256 * 1024:
                        raise RuntimeError("unexpected oversized owned response")
                    result = json.loads(exact(stream, length))
            else:
                request_id += 1
                code = "return await call(%s, 'ssmcp.ui.listener', %s);" % (
                    json.dumps(config["client_id"]), json.dumps(arguments, ensure_ascii=False))
                call = {"code": code}
                if command.get("agent") is not None:
                    call["agent"] = command["agent"]
                params = {"name": "code_mode", "arguments": call,
                          "_meta": {PROTOCOL_META: MODERN, CAPABILITIES_META: {}}}
                payload = {"jsonrpc": "2.0", "id": request_id,
                           "method": "tools/call", "params": params}
                connection = http.client.HTTPConnection("127.0.0.1", config["port"], timeout=12)
                headers = {"Content-Type": "application/json", "Accept": "application/json, text/event-stream",
                           "MCP-Protocol-Version": MODERN, "Mcp-Method": "tools/call", "Mcp-Name": "code_mode"}
                token = "" if command.get("unauthenticated") else config["token"]
                if token:
                    headers["Authorization"] = "Bearer " + token
                try:
                    connection.request("POST", "/mcp", json.dumps(payload).encode(), headers)
                    response = connection.getresponse()
                    raw = response.read(256 * 1024 + 1)
                    if len(raw) > 256 * 1024:
                        raise RuntimeError("unexpected oversized owned HTTP response")
                    # Authentication rejection may deliberately have no JSON body.
                    # Assert the real HTTP status before decoding a success envelope.
                    if response.status != 200:
                        result = {"ok": False, "http_status": response.status}
                    else:
                        body = json.loads(raw)
                        if "error" in body:
                            print(json.dumps({"ok": False, "http_status": response.status}), flush=True)
                            continue
                        envelope = body["result"]
                        native = envelope if envelope.get("isError") else envelope["structuredContent"]["result"]
                        if native.get("isError"):
                            result = {"ok": False, "error": " ".join(
                                item.get("text", "") for item in native.get("content", []) if item.get("type") == "text")}
                        else:
                            result = {"ok": True, "result": native["structuredContent"]}
                finally:
                    connection.close()
            print(json.dumps(result, ensure_ascii=False), flush=True)
        except Exception as error:
            print(json.dumps({"ok": False, "worker_error": type(error).__name__}), flush=True)


class Program:
    def __init__(self, harness, mode, config):
        self.process = subprocess.Popen([sys.executable, str(Path(__file__).resolve()), "--worker", mode],
                                        stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                                        text=True, start_new_session=True)
        harness.processes.append(self.process)
        self.process.stdin.write(json.dumps(config) + "\n")
        self.process.stdin.flush()

    def send(self, arguments, **options):
        self.process.stdin.write(json.dumps({"arguments": arguments, **options}) + "\n")
        self.process.stdin.flush()

    def receive(self):
        if not select.select([self.process.stdout], [], [], 15)[0]:
            raise AssertionError("owned listener program timed out")
        line = self.process.stdout.readline()
        if not line:
            raise AssertionError("owned listener program exited")
        return json.loads(line)

    def call(self, op, **arguments):
        self.send({"op": op, **arguments})
        return self.receive()

    def close(self):
        self.process.stdin.write('{"stop":true}\n')
        self.process.stdin.flush()
        self.process.stdin.close()
        if self.process.wait(timeout=5) != 0:
            raise AssertionError("owned listener program failed to exit")


class Transports(ForceSend):
    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        subprocess.run(["/usr/bin/git", "-c", "init.defaultBranch=main", "init", "-q"],
                       cwd=self.workspace, env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5)
        front = self.start_front("listener-transports")
        front.resize(30, 120)
        front.until(lambda shot: shot["size"] == [30, 120], "owned listener terminal")
        deadline = time.monotonic() + 5
        endpoint = None
        while time.monotonic() < deadline:
            endpoint = self.listener("status").get("local_endpoint")
            if endpoint:
                break
            front.pump(0.03)
        self.check(bool(endpoint), "the actual client publishes its private Unix listener endpoint")
        endpoint = Path(endpoint)
        self.check(stat.S_IMODE(endpoint.stat().st_mode) == 0o600
                   and stat.S_IMODE(endpoint.parent.stat().st_mode) == 0o700
                   and endpoint.stat().st_uid == os.geteuid(),
                   "the owned socket and directory have exact same-UID private permissions")
        network = Program(self, "http", {"client_id": self.client_id, "port": self.http_port, "token": self.secret})
        local = Program(self, "unix", {"endpoint": str(endpoint)})
        self.check(len({network.process.pid, local.process.pid, os.getpid()}) == 3,
                   "HTTP and Unix consumers are two independent real programs")
        network.send({"op": "status"}, unauthenticated=True)
        denied = network.receive()
        self.check(not denied["ok"] and denied.get("http_status") in (401, 403),
                   "the real HTTP listener ingress rejects a missing OAuth credential")
        opened = network.call("open", label="MCP program", idempotency_key="network_1")
        self.check(opened["ok"], "the authenticated network program acquires the main-thread lease")
        lease = {key: opened["result"][key] for key in ("listener_id", "token")}
        self.sensitive.append(lease["token"])
        again = network.call("open", label="MCP program", idempotency_key="network_1")
        self.check(again["ok"] and again["result"]["token"] == lease["token"],
                   "same-owner network registration retry keeps the exact original lease")
        other = local.call("open", label="Unix program", idempotency_key="network_1")
        self.check(not other["ok"] and "listener_busy" in other["error"],
                   "a second real program cannot replace the active network lease or reuse its key")
        ordinary = self.submit_source(front, "Ordinary queued input remains separate")
        network.send({"op": "read", **lease, "wait_ms": 5000})
        deadline = time.monotonic() + 4
        while time.monotonic() < deadline and not self.listener("status")["listener"]["reading"]:
            front.pump(0.02)
        self.check(self.listener("status")["listener"]["reading"],
                   "the HTTP program is genuinely waiting in a bounded long poll")
        busy = local.call("read", **lease, wait_ms=0)
        self.check(not busy["ok"] and "read_busy" in busy["error"],
                   "the Unix adapter respects the exact same pending-reader exclusion")
        delivery_id = self.force(front, "/send Shared Unicode 消息\n    indented")
        remote_delivery = network.receive()
        self.check(remote_delivery["ok"], "a real forced PTY message wakes the network long poll")
        local_delivery = local.call("read", **lease, wait_ms=0)
        self.check(local_delivery == remote_delivery
                   and local_delivery["result"]["delivery"]["delivery_id"] == delivery_id
                   and local_delivery["result"]["delivery"]["cursor"] == 1,
                   "both transports return the same complete body, delivery ID and cursor")
        ack = local.call("ack", **lease, delivery_id=delivery_id, status="accepted")
        self.check(ack["ok"], "an explicitly delegated same-UID Unix program acknowledges the shared delivery")
        self.receipt(front, delivery_id, "accepted")
        duplicate = network.call("ack", **lease, delivery_id=delivery_id, status="accepted")
        self.check(duplicate == ack, "the network retry sees the identical final Unix ACK receipt")
        self.ordinary_unchanged(ordinary)
        self.check(network.call("read", **lease, wait_ms=0)["result"]["delivery"] is None,
                   "an acknowledged delivery cannot be independently consumed again on the other transport")
        self.check(network.call("close", **lease)["ok"], "the network program closes its own lease")
        expired = network.call("open", label="MCP program", idempotency_key="network_1")
        self.check(not expired["ok"] and "idempotency_expired" in expired["error"],
                   "a closed idempotent registration retry cannot silently mint a replacement lease")
        opened = local.call("open", label="Unix program", idempotency_key="unix_1")
        self.check(opened["ok"], "the local program can explicitly register after the prior owner closes")
        local_lease = {key: opened["result"][key] for key in ("listener_id", "token")}
        self.sensitive.append(local_lease["token"])
        forbidden = network.call("read", **local_lease, wait_ms=0)
        self.check(not forbidden["ok"], "even a copied local token cannot fabricate a remote account-owned lease")
        second = self.force(front, "/send Local program receives cursor two")
        read = local.call("read", **local_lease, wait_ms=0)
        self.check(read["ok"] and read["result"]["delivery"]["cursor"] == 2,
                   "a replacement program advances the same connection cursor")
        local.call("close", **local_lease)
        self.receipt(front, second, "unknown")
        third = network.call("open", label="New explicit lease", idempotency_key="network_2")
        third_lease = {key: third["result"][key] for key in ("listener_id", "token")}
        self.sensitive.append(third_lease["token"])
        self.check(network.call("read", **third_lease, wait_ms=0)["result"]["delivery"] is None,
                   "an offered but unacknowledged old message is unknown and never auto-replayed")
        network.call("close", **third_lease)
        self.ordinary_unchanged(ordinary)
        # Remove only the fixture's own ordinary request through its public CAS
        # cancellation before testing an explicitly bound code_mode participant.
        self.observe("ssmcp.ui.queue", {"action": "cancel", "id": ordinary["id"]})
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            record = next((row for row in self.queue()["queue"] if row["id"] == ordinary["id"]), None)
            if record is None or record["status"] == "cancelled":
                break
            front.pump(0.03)
        self.check(record is None or record["status"] == "cancelled",
                   "the fixture explicitly cancels only its own ordinary request before binding an agent")
        member = self.public_call("collab", {"action": "join", "client_id": self.client_id,
                                             "name": "Bound listener fixture"})
        self.sensitive.append(member["token"])
        agent = {"client_id": self.client_id, "id": member["agent_id"], "token": member["token"]}
        network.send({"op": "open", "label": "Bound MCP program", "idempotency_key": "bound_1"}, agent=agent)
        bound = network.receive()
        self.check(bound["ok"] and bound["result"]["thread_id"] == member["agent_id"],
                   "real broker capability negotiation carries the verified participant into the listener scope")
        bound_lease = {key: bound["result"][key] for key in ("listener_id", "token")}
        self.sensitive.append(bound_lease["token"])
        self.check(not network.call("read", **bound_lease, wait_ms=0)["ok"],
                   "an unbound call from the same actual account cannot steal a bound-thread delivery")
        self.draft(front, "/send Main must not reach private agent")
        front.send(ENTER)
        front.until(lambda shot: "线程不同" in text(shot), "main-thread mismatch feedback")
        self.check(self.listener("status")["pending"] is None,
                   "a main-thread draft is not silently redirected to the agent listener")
        self.draft(front, "")
        # Registration and the client's one-second steering snapshots are
        # independent. Wait for the real legacy thread row; the native message
        # row is a different channel and must never stand in for this listener.
        deadline = time.monotonic() + 8
        roster = []
        matching = []
        while time.monotonic() < deadline:
            front.pump(0.03)
            roster = self.queue()["agents"]
            matching = [index for index, entry in enumerate(roster)
                        if entry["agent_id"] == member["agent_id"]]
            if matching:
                break
        if len(matching) != 1:
            raise AssertionError(
                "the registered listener thread did not appear exactly once in the real steering roster; "
                + json.dumps({"expected_agent_id": member["agent_id"], "observed_agents": roster},
                             ensure_ascii=False)
            )
        self.check(roster[matching[0]]["agent_id"] == member["agent_id"],
                   "the real steering snapshot publishes the exact registered listener thread")
        self.agents(front)
        selected = 1 + matching[0]
        front.send(HOME + DOWN * selected + ENTER)
        self.wait_state(front, lambda state: state["target"] == member["agent_id"]
                        and state["native_recipient"] is None,
                        "the real keyboard selection did not choose the exact listener thread")
        delivery_id = self.force(front, "/send Only the selected agent receives this")
        network.send({"op": "read", **bound_lease, "wait_ms": 0}, agent=agent)
        read = network.receive()
        self.check(read["ok"] and read["result"]["delivery"]["delivery_id"] == delivery_id
                   and read["result"]["delivery"]["cursor"] == 3,
                   "the real selected-agent path delivers only to its immutable registered scope")
        network.send({"op": "ack", **bound_lease, "delivery_id": delivery_id, "status": "accepted"}, agent=agent)
        self.check(network.receive()["ok"], "the same verified participant acknowledges its own delivery")
        self.receipt(front, delivery_id, "accepted")
        network.send({"op": "close", **bound_lease}, agent=agent)
        self.check(network.receive()["ok"], "the bound program closes with its original verified scope")
        self.public_call("collab", {"action": "leave", "client_id": self.client_id,
                                    "agent_id": member["agent_id"], "token": member["token"]})
        self.save_shot("listener-two-programs", front, cells=True)
        network.close()
        local.close()
        self.draft(front, "")
        final, restored = self.disconnect_front(front)
        self.check(not final["alternate_screen"] and restored == front.original_termios,
                   "the listener fixture restores its owned PTY exactly")
        deadline = time.monotonic() + 3
        while endpoint.parent.exists() and time.monotonic() < deadline:
            time.sleep(0.02)
        self.check(not endpoint.exists() and not endpoint.parent.exists(),
                   "disconnect removes the owned socket, lock and unique directory")
        self.check(network.process.poll() == 0 and local.process.poll() == 0,
                   "both independent listener programs terminate without owned descendants")


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("listener E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--worker", choices=("http", "unix"))
    parser.add_argument("--bin-dir", type=Path)
    parser.add_argument("--decoder", type=Path)
    parser.add_argument("--out", type=Path)
    args = parser.parse_args()
    if args.worker:
        worker(args.worker)
        return
    if not all((args.bin_dir, args.decoder, args.out)):
        parser.error("--bin-dir, --decoder and --out are required")
    args.out.mkdir(parents=True, exist_ok=True)
    digest = lambda path: hashlib.sha256(Path(path).read_bytes()).hexdigest()
    report = {"status": "failed", "binary_sha256": digest(args.bin_dir / "ssmcp"),
              "script_sha256": digest(__file__), "scope": "real OAuth/MCP + two real programs + private Unix + PTY"}
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-listener-transports-") as directory:
        harness = Transports(Path(directory), args.bin_dir, args.decoder, args.out)
        try:
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            message = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    message = message.replace(secret, "[redacted]")
            report["error"] = message
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
            owned = list(dict.fromkeys([*harness.processes, harness.account_gateway.process]))
            report["cleanup"] = {
                "owned_processes": [{"pid": item.pid, "exit_code": item.poll()} for item in owned],
                "all_exited": all(item.poll() is not None for item in owned),
            }
            if not report["cleanup"]["all_exited"]:
                report["status"] = "failed"
                report["error"] = "an owned process remained after cleanup"
            report.update(checks=harness.checks, seconds=round(time.monotonic() - started, 3))
            (args.out / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]), "error": report.get("error")}))
    raise SystemExit(report["status"] != "passed")


if __name__ == "__main__":
    main()
