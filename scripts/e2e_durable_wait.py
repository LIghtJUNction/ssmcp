#!/usr/bin/env python3
"""Accept durable awaits and timed prompts through an owned real account router.

Requires prebuilt, hash-pinned client/router binaries. The default command runs
for 600 seconds exactly once. --duration-seconds permits a shorter diagnostic;
the report labels that run as diagnostic, never as the 600-second acceptance.
Only HTTP observation is interrupted. The native session is never restarted.
No Cargo, downloads, installed account or model is used.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import http.client
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess
import sys
import tempfile
import time
import uuid

from e2e import CAPABILITIES_META, MODERN, PROTOCOL_META, Harness


def digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


def file_identity(path):
    path = Path(path).resolve(strict=True)
    return {"path": str(path), "bytes": path.stat().st_size, "sha256": digest(path)}


def redact(value, secrets):
    text = json.dumps(value, ensure_ascii=False, default=str)
    for secret in sorted((s for s in secrets if isinstance(s, str) and s), key=len, reverse=True):
        text = text.replace(secret, "[redacted]")
    return json.loads(text)


def atomic_json(path, value):
    temporary = path.with_name(path.name + ".tmp")
    with temporary.open("w", encoding="utf-8") as stream:
        os.chmod(temporary, 0o600)
        json.dump(value, stream, ensure_ascii=False, indent=2)
        stream.write("\n")
        stream.flush()
        os.fsync(stream.fileno())
    os.replace(temporary, path)


def mappings(value):
    if isinstance(value, dict):
        yield value
        for item in value.values():
            yield from mappings(item)
    elif isinstance(value, list):
        for item in value:
            yield from mappings(item)


class CapabilityUnavailable(RuntimeError):
    pass


class DurableWait(Harness):
    def __init__(self, directory, options, report):
        binary_dir = directory / "bin"
        binary_dir.mkdir(mode=0o700)
        (binary_dir / "ssmcp").symlink_to(options.client_binary)
        super().__init__(directory, binary_dir)
        self.options = options
        self.report = report
        self.fixture_binary = options.fixture_binary
        self.fixture_timeout_seconds = min(1800, options.duration_seconds + 180)
        self.http_timeout = 15
        self.workspace = directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        self.member = None
        self.wait_id = None
        self.job_id = None
        self.timer_ids = []
        self.http_requests = 0
        self.exec_dispatches = 0
        self.wait_acked = False
        self.native_tools = {}
        self.journal = options.evidence / "journal.jsonl"

    def request(self, *args, **kwargs):
        self.http_requests += 1
        return super().request(*args, **kwargs)

    def record(self, event, **value):
        entry = {"event": event, "at_unix_ms": time.time_ns() // 1_000_000,
                 "monotonic_ns": time.monotonic_ns(), **value}
        entry = redact(entry, [self.secret, *self.sensitive])
        with self.journal.open("a", encoding="utf-8") as stream:
            os.chmod(self.journal, 0o600)
            stream.write(json.dumps(entry, ensure_ascii=False) + "\n")
            stream.flush()
            os.fsync(stream.fileno())

    def checkpoint(self, **fields):
        self.report.update(fields)
        self.report["checks"] = list(self.checks)
        self.report["http_requests"] = self.http_requests
        atomic_json(self.options.report, redact(self.report, [self.secret, *self.sensitive]))

    @property
    def agent(self):
        return {"id": self.member["agent_id"], "token": self.member["token"]}

    def client_environment(self, label):
        environment = super().client_environment(label)
        owned = {"SSMCP_AUTH_HOME", "SSMCP_SESSION_HOME", "SSMCP_CLIENT_CONFIG", "SSMCP_UI_CONFIG"}
        return {key: value for key, value in environment.items()
                if not key.startswith("SSMCP_") or key in owned}

    def response(self, name, arguments):
        status, reply, _ = self.rpc("tools/call", {"name": name, "arguments": arguments},
                                    modern=True)
        self.last_public_reply = reply
        self.record("rpc_response", tool=name, http_status=status, response=reply)
        if status != 200 or not isinstance(reply, dict):
            raise RuntimeError(f"{name} transport outcome uncertain: HTTP {status}; do not replay exec")
        return reply

    def tool(self, name, arguments):
        reply = self.response(name, arguments)
        result = reply.get("result", {})
        if "error" in reply or result.get("isError"):
            raise AssertionError(f"{name} rejected: {reply}")
        value = result.get("structuredContent")
        if not isinstance(value, dict):
            raise AssertionError(f"{name} omitted structuredContent")
        return value

    def native_reply(self, name, arguments):
        code = "await collab.bind(%s, %s); return await call(%s, %s, %s);" % (
            json.dumps(self.client_id),
            json.dumps({"agent_id": self.member["agent_id"], "token": self.member["token"]}),
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments))
        return self.tool("code_mode", {"code": code, "timeout_ms": 10000})

    def native(self, name, arguments=None):
        outer = self.native_reply(name, arguments or {})
        value = outer.get("result", {})
        if value.get("isError") or not isinstance(value.get("structuredContent"), dict):
            raise AssertionError(f"Native {name} failed: {value}")
        body = value["structuredContent"]
        if body.get("spilled") is True:
            artifact = body.get("artifact", {})
            if (body.get("format") != "mcp_tool_result_json"
                    or artifact.get("complete") is not True
                    or not isinstance(artifact.get("bytes"), int)
                    or not 0 < artifact["bytes"] <= 1_048_576
                    or "ssmcp.output.read" not in self.native_tools):
                raise AssertionError("original native result spill cannot be read completely")
            data, offset = bytearray(), 0
            while offset < artifact["bytes"]:
                original = self.native_reply("ssmcp.output.read",
                    {"id": artifact["id"], "offset": offset, "max_bytes": 4096}).get("result", {})
                page = original.get("structuredContent", {})
                if original.get("isError") or page.get("spilled") or not page.get("data_base64"):
                    raise AssertionError("original native result artifact is unavailable; no tool replay")
                chunk = base64.b64decode(page["data_base64"], validate=True)
                if page.get("next_offset") != offset + len(chunk):
                    raise AssertionError("original native result artifact cursor is inconsistent")
                data.extend(chunk)
                offset = page["next_offset"]
                if page.get("eof"):
                    break
            if len(data) != artifact["bytes"] or hashlib.sha256(data).hexdigest() != artifact.get("sha256"):
                raise AssertionError("original native result artifact bytes do not match its receipt")
            restored = json.loads(data)
            if restored.get("isError") or not isinstance(restored.get("structuredContent"), dict):
                raise AssertionError("restored original native tool result is not a successful object")
            body = restored["structuredContent"]
            self.record("original_native_result_restored", tool=name,
                        artifact_id=artifact["id"], bytes=len(data), sha256=artifact["sha256"])
        return body

    def await_call(self, **arguments):
        return self.tool("await", {"client_id": self.client_id, "agent": self.agent, **arguments})

    def metadata(self):
        value = self.await_call(op="status", wait_id=self.wait_id)
        self.check(value.get("wait_id") == self.wait_id and value.get("metadata_only") is True
                   and value.get("results") == [], "status returns metadata without consuming result")
        return value

    def no_http_until(self, deadline, label):
        count = self.http_requests
        began = time.monotonic()
        self.record("http_absence_started", label=label, http_requests=count)
        while time.monotonic() < deadline:
            time.sleep(min(20, max(0, deadline - time.monotonic())))
            print(f"WAIT {label}: {time.monotonic() - began:.1f}s without HTTP observation", flush=True)
        self.check(self.http_requests == count, label + ": no HTTP observer polls during absence")
        self.record("http_absence_ended", label=label, http_requests=self.http_requests,
                    absent_seconds=time.monotonic() - began)

    def abandon_observation(self, before_windows):
        self.request_id += 1
        arguments = {"client_id": self.client_id, "agent": self.agent,
                     "wait_id": self.wait_id, "after": 0, "window_ms": 10000}
        payload = {"jsonrpc": "2.0", "id": self.request_id, "method": "tools/call",
                   "params": {"name": "await", "arguments": arguments,
                              "_meta": {PROTOCOL_META: MODERN, CAPABILITIES_META: {}}}}
        connection = http.client.HTTPConnection("127.0.0.1", self.http_port, timeout=5)
        self.record("abandoned_observation_dispatch", wait_id=self.wait_id,
                    rpc_id=self.request_id, response_outcome="unknown")
        self.http_requests += 1
        try:
            connection.request("POST", f"/{self.user}", body=json.dumps(payload).encode(), headers={
                "Accept": "application/json, text/event-stream", "Content-Type": "application/json",
                "Authorization": "Bearer " + self.secret, "MCP-Protocol-Version": MODERN,
                "Mcp-Method": "tools/call", "Mcp-Name": "await"})
            # Observe admission of this exact request, then drop its unread
            # response while its ten-second observation window is still pending.
            # Metadata reads neither replay execution nor acknowledge its body.
            deadline = time.monotonic() + 2
            while True:
                admitted = self.metadata()
                if admitted["metadata"]["read_windows"] >= before_windows + 1:
                    self.check(admitted.get("status") == "pending"
                               and admitted["metadata"].get("first_read_at_ms") is None,
                               "the original abandoned observation is admitted before disconnect")
                    break
                if time.monotonic() >= deadline:
                    raise AssertionError("original observation admission remains unknown; no wait or exec replay")
                time.sleep(0.02)
            if connection.sock is not None:
                try:
                    connection.sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass
        finally:
            connection.close()
        self.record("abandoned_observation_closed", wait_id=self.wait_id,
                    response_outcome="unknown", response_read=False)

    def prepare_account(self):
        self.prepare()
        status, account, _ = self.request("GET", path="/api/client/whoami", secret=self.account_token)
        self.check(status == 200, "fixture account authentication succeeds")
        self.account_gateway.validate_account_identity(account.get("identity"))
        self.client, self.credentials = self.start_client(name="durable-wait-fixture", cwd=self.workspace)
        self.client_id = self.credentials["client_id"]
        self.oauth_login(self.credentials)
        self.member = self.public_call("collab", {"action": "join", "client_id": self.client_id,
                                                  "name": "durable-wait-fixture", "role": "worker"})
        self.sensitive.append(self.member["token"])
        self.original_identity = {"client_id": self.client_id, "agent_id": self.member["agent_id"],
                                  "client_pid": self.client.pid, "account": self.identity.copy()}
        self.checkpoint(native_identity=self.original_identity, status="discovering")
        self.record("joined_owned_participant", **self.original_identity)
        status, reply, _ = self.rpc("tools/list", modern=True)
        self.check(status == 200 and isinstance(reply.get("result", {}).get("tools"), list),
                   "public tools/list returns actual schemas")
        public = reply["result"]["tools"]
        self.native_tools = {item["name"]: item for item in self.native_catalog(self.client_id)}
        self.report["schema_artifacts"] = []
        for label, value in (("public-tools", public), ("native-tools", self.native_tools)):
            path = self.options.evidence / (label + ".json")
            atomic_json(path, value)
            self.report["schema_artifacts"].append(file_identity(path))
        descriptor = next((item for item in public if item.get("name") == "await"), None)
        properties = (descriptor or {}).get("inputSchema", {}).get("properties", {})
        if (not {"request_id", "lifetime_ms", "wait_id", "after", "window_ms"} <= properties.keys()
                or not {"list", "status", "wait", "cancel"} <= set(properties.get("op", {}).get("enum", []))):
            raise CapabilityUnavailable("prebuilt router does not advertise durable await schema")
        self.check(properties["lifetime_ms"].get("maximum") == 86_400_000,
                   "durable await advertises its exact maximum lifetime")
        for name in ("ssmcp.exec.start", "ssmcp.exec.read", "ssmcp.exec.list"):
            if name not in self.native_tools:
                raise CapabilityUnavailable("missing native capability " + name)
        jobs = self.native("ssmcp.exec.list")["jobs"]
        self.check(len(jobs) == 1 and jobs[0].get("job_id") == "main"
                   and jobs[0].get("mode") == "pty" and jobs[0].get("running") is True
                   and jobs[0].get("command") == ["/bin/sh", "-i"],
                   "fresh owned native session contains only its original interactive shell")
        self.baseline_main_pid = jobs[0]["pid"]
        self.report["baseline_shell"] = {key: jobs[0].get(key) for key in
                                         ("job_id", "pid", "mode", "command", "running")}

    def conflict(self, original, field, value):
        changed = {**original, field: value}
        reply = self.response("await", {"client_id": self.client_id, "agent": self.agent, **changed})
        result = reply.get("result", {})
        body = result.get("structuredContent", {})
        self.check(body.get("status") == "unavailable"
                   and body.get("error", {}).get("code") == "await_request_conflict"
                   and body.get("wait_id") is None,
                   f"same request_id rejects immutable {field} changes with the exact conflict code")
        self.report.setdefault("creation_conflicts", []).append({"field": field, "reply": reply})

    def durable_case(self):
        marker = "DURABLE_" + uuid.uuid4().hex
        marker_path = self.workspace / "native-once.jsonl"
        child = (
            "import json,os,pathlib,sys,time\n"
            "path=pathlib.Path(sys.argv[1]);marker=sys.argv[2];duration=int(sys.argv[3])\n"
            "fd=os.open(path,os.O_WRONLY|os.O_CREAT|os.O_EXCL,0o600)\n"
            "with os.fdopen(fd,'w') as f:\n"
            " f.write(json.dumps({'event':'started','marker':marker,'pid':os.getpid(),"
            "'monotonic_ns':time.monotonic_ns(),'unix_ms':time.time_ns()//1000000})+'\\n');f.flush();os.fsync(f.fileno())\n"
            "print(marker,flush=True)\n"
            "time.sleep(duration)\n"
            "with path.open('a') as f:\n"
            " f.write(json.dumps({'event':'completed','pid':os.getpid(),"
            "'monotonic_ns':time.monotonic_ns(),'unix_ms':time.time_ns()//1000000})+'\\n');f.flush();os.fsync(f.fileno())\n"
            "print('DURABLE_COMMAND_COMPLETED',flush=True)\n"
        )
        command = [sys.executable, "-u", "-c", child, str(marker_path), marker,
                   str(self.options.duration_seconds)]
        arguments = {"command": command, "cwd": str(self.workspace), "mode": "pipes",
                     "title": "Durable-await acceptance: one bounded native command",
                     "timeout_ms": (self.options.duration_seconds + 30) * 1000}
        self.check(self.exec_dispatches == 0, "exec.start has not been dispatched previously")
        self.exec_dispatches += 1
        self.record("exec_dispatch", command=command, cwd=str(self.workspace),
                    timeout_ms=arguments["timeout_ms"], outcome="unknown_until_receipt")
        self.checkpoint(status="exec_dispatch", execution={"dispatches": self.exec_dispatches,
                        "command": command, "outcome": "unknown_until_receipt"})
        started = self.native("ssmcp.exec.start", arguments)
        self.job_id = started["job_id"]
        exec_receipt_at = time.monotonic()
        self.record("exec_started", receipt=started, job_id=self.job_id)
        self.checkpoint(status="creating_durable_wait", execution={
            "dispatches": self.exec_dispatches, "command": command, "job_id": self.job_id,
            "start_receipt": started, "outcome": "started"})
        create = {"request_id": "durable_" + uuid.uuid4().hex, "lifetime_ms":
                  (self.options.duration_seconds + 60) * 1000,
                  "window_ms": 1000, "mode": "any",
                  "targets": [{"type": "exec", "job_id": self.job_id}]}
        self.create_arguments = create
        create_sent_ns = time.monotonic_ns()
        first = self.await_call(**create)
        create_returned_ns = time.monotonic_ns()
        self.wait_id = first["wait_id"]
        self.check(first.get("status") == "pending" and first.get("cursor") == 0
                   and first.get("results") == [] and first.get("metadata_only") is True,
                   "durable creation returns pending metadata during the original command")
        self.check(first.get("request_id") == create["request_id"]
                   and first["metadata"]["lifetime_ms"] == create["lifetime_ms"],
                   "durable creation preserves its immutable request identity and lifetime")
        self.checkpoint(status="durable_wait_created", wait_id=self.wait_id,
                        request_id=create["request_id"], create_receipt=first)
        duplicate = self.await_call(**{**create, "window_ms": 2000})
        self.check(duplicate.get("wait_id") == self.wait_id and duplicate.get("status") == "pending",
                   "same request_id with another read window returns the same live wait")
        self.conflict(create, "targets", [{"type": "exec", "job_id": self.job_id + "x"}])
        self.conflict(create, "mode", "all")
        self.conflict(create, "lifetime_ms", create["lifetime_ms"] + 1000)
        before = self.metadata()
        self.check(before["metadata"].get("first_read_at_ms") is None,
                   "pending metadata inspection is not a terminal result read")
        before_windows = before["metadata"]["read_windows"]
        self.abandon_observation(before_windows)
        self.checkpoint(status="http_absent", observer_abandoned=True,
                        abandoned_response_outcome="unknown", exec_dispatches=self.exec_dispatches)
        self.no_http_until(exec_receipt_at + self.options.duration_seconds + 5,
                           "durable command continues after HTTP abandonment")

        # Reauthenticate the same synthetic account over new HTTP connections.
        # The same native process and member credentials remain in place.
        self.oauth_login(self.credentials)
        self.check(self.client.poll() is None and self.client.pid == self.original_identity["client_pid"]
                   and self.client_id == self.original_identity["client_id"]
                   and self.member["agent_id"] == self.original_identity["agent_id"]
                   and self.identity == self.original_identity["account"],
                   "HTTP reconnect retains the same verified account, agent and native process")
        clients = self.public_call("list")["clients"]
        self.check(any(value.get("id") == self.client_id for value in clients),
                   "the original native client ID remains authorized after HTTP reconnect")
        terminal_meta = self.metadata()
        metadata = terminal_meta["metadata"]
        self.check(terminal_meta.get("status") == "ready" and terminal_meta.get("cursor") == 1,
                   "durable observer completed while HTTP observation was absent")
        self.check(metadata.get("first_read_at_ms") is None and metadata["read_windows"] >= before_windows + 1,
                   "abandoned observation was admitted but metadata inspection did not read cached body")
        self.check(metadata["lifetime_ms"] == create["lifetime_ms"]
                   and isinstance(metadata["completed_at_ms"], int)
                   and metadata["result_status"] == "ready",
                   "terminal metadata retains the original lifetime and completion result")
        self.check(isinstance(metadata["retained_until_ms"], int)
                   and metadata["retained_until_ms"] - metadata["completed_at_ms"] >= 1_800_000,
                   "terminal metadata declares at least thirty minutes of cache retention")
        listed = self.await_call(op="list")
        self.check(listed.get("metadata_only") is True and isinstance(listed.get("waits"), list),
                   "await list is an explicit metadata-only operation")
        matches = [item for item in listed["waits"] if item.get("wait_id") == self.wait_id]
        self.check(len(matches) == 1 and len(listed["waits"]) == 1,
                   "duplicate and conflicting creation attempts produced no second wait")

        cached = self.await_call(wait_id=self.wait_id, window_ms=1000, after=0)
        self.check(cached.get("status") == "ready" and cached.get("cursor") == 1
                   and len(cached.get("results", [])) == 1,
                   "reconnected observer retrieves terminal cached result at cursor 1")
        target = cached["results"][0]
        self.check(target.get("target") == create["targets"][0] and target.get("status") == "ready",
                   "cached result belongs to the one original execution target")
        native = target.get("data", {})
        self.check(native.get("job_id") == self.job_id and native.get("running") is False
                   and native.get("exit_code") == 0,
                   "cached native exec.wait receipt confirms the original command exited zero")
        cached_metadata = cached["metadata"]
        self.check(isinstance(cached_metadata["delivery_lag_ms"], int)
                   and cached_metadata["delivery_lag_ms"] >= 0
                   and isinstance(cached_metadata["first_read_at_ms"], int),
                   "first cached body read records delivery lag and read timestamp")
        replay = self.await_call(wait_id=self.wait_id, window_ms=1000, after=0)
        self.check(replay.get("wait_id") == self.wait_id and replay.get("cursor") == 1
                   and replay.get("results") == cached["results"],
                   "unacknowledged terminal body replays unchanged")
        self.check(replay["metadata"]["elapsed_total_ms"] == cached_metadata["elapsed_total_ms"]
                   and replay["metadata"]["first_read_at_ms"] == cached_metadata["first_read_at_ms"]
                   and replay["metadata"]["replay_count"] >= cached_metadata["replay_count"] + 1,
                   "replay increments its count without inflating original completion elapsed time")
        acknowledged = self.await_call(wait_id=self.wait_id, window_ms=1000, after=1)
        self.check(acknowledged.get("acknowledged") is True and acknowledged.get("cursor") == 1
                   and acknowledged.get("results") == [] and acknowledged.get("status") == "ready",
                   "explicit after:1 acknowledges only the retained await snapshot")
        self.wait_acked = True
        ack_again = self.await_call(wait_id=self.wait_id, after=1)
        self.check(ack_again.get("acknowledged") is True
                   and ack_again["metadata"]["acknowledged_at_ms"] ==
                   acknowledged["metadata"]["acknowledged_at_ms"],
                   "repeated ACK preserves its original acknowledgment timestamp")
        expired = self.response("await", {"client_id": self.client_id, "agent": self.agent,
                                         "wait_id": self.wait_id, "window_ms": 1000, "after": 0})
        expired_result = expired.get("result", {})
        self.check(("error" in expired or expired_result.get("isError") or
                    expired_result.get("structuredContent", {}).get("error") is not None)
                   and "await result cursor expired after acknowledgement" in json.dumps(expired),
                   "after:0 following ACK rejects the exact expired result cursor")
        acknowledged_create = self.await_call(**create)
        self.check(acknowledged_create.get("wait_id") == self.wait_id
                   and acknowledged_create.get("request_id") == create["request_id"]
                   and acknowledged_create.get("cursor") == 1
                   and acknowledged_create.get("status") == "ready"
                   and acknowledged_create.get("metadata_only") is True
                   and acknowledged_create.get("results") == []
                   and acknowledged_create["metadata"]["state"] == "acknowledged"
                   and acknowledged_create["metadata"]["acknowledged_at_ms"] ==
                   acknowledged["metadata"]["acknowledged_at_ms"],
                   "creation retry after ACK returns metadata for the same acknowledged wait")

        output = self.native("ssmcp.exec.read", {"job_id": self.job_id, "stdout_cursor": 0,
                                               "stderr_cursor": 0, "max_bytes": 32768, "wait_ms": 0})
        self.check(output.get("running") is False and output.get("exit_code") == 0
                   and output.get("stdout_eof") is True and output.get("stderr_eof") is True,
                   "original command has an exit status and EOF on both streams")
        self.check(output.get("stdout_truncated") is False and output.get("stderr_truncated") is False,
                   "command evidence includes complete untruncated streams")
        stdout = base64.b64decode(output["stdout_base64"])
        stderr = base64.b64decode(output["stderr_base64"])
        self.check(stdout.decode() == output["stdout"] and stderr.decode() == output["stderr"],
                   "native output text agrees with exact base64 stream bytes")
        self.check(stdout.decode().splitlines() == [marker, "DURABLE_COMMAND_COMPLETED"] and not stderr,
                   "the native command emitted its unique marker exactly once")
        lines = [json.loads(line) for line in marker_path.read_text().splitlines()]
        self.check(len(lines) == 2 and lines[0]["event"] == "started" and lines[1]["event"] == "completed"
                   and lines[0]["marker"] == marker and lines[0]["pid"] == lines[1]["pid"],
                   "exclusive marker file contains exactly one start and one completion")
        duration = (lines[1]["monotonic_ns"] - lines[0]["monotonic_ns"]) / 1e9
        self.check(duration >= self.options.duration_seconds,
                   "native monotonic timestamps cover the requested command duration")
        lower = max(0, (lines[1]["monotonic_ns"] - create_returned_ns) // 1_000_000 - 1000)
        upper = (time.monotonic_ns() - create_sent_ns) // 1_000_000 + 1000
        self.check(lower <= cached_metadata["elapsed_total_ms"] <= upper,
                   "cached elapsed time agrees with the actual native completion interval")
        jobs = self.native("ssmcp.exec.list")["jobs"]
        self.check(len(jobs) == 2 and {job["job_id"] for job in jobs} == {"main", self.job_id}
                   and next(job["pid"] for job in jobs if job["job_id"] == "main")
                   == self.baseline_main_pid and self.exec_dispatches == 1,
                   "reconnect, replay, conflicts and ACK retain the original shell and one native execution")
        for name, data in (("native.stdout.log", stdout), ("native.stderr.log", stderr),
                           ("native-marker.jsonl", marker_path.read_bytes())):
            path = self.options.evidence / name
            path.write_bytes(data)
            path.chmod(0o600)
        self.checkpoint(status="durable_case_passed", durable_case={
            "status": "passed", "job_id": self.job_id, "wait_id": self.wait_id,
            "native_duration_seconds": duration, "cached_receipt": cached,
            "replay_receipt": replay, "ack_receipt": acknowledged,
            "metadata_before_body_read": terminal_meta, "output_receipt": output,
            "exec_dispatches": self.exec_dispatches})

    def prompt(self, **arguments):
        value = self.native("ssmcp.time.prompt", arguments)
        self.check(value.get("schema_version") == 1 and value.get("provenance") == "agent_timer"
                   and value.get("scope") == "native_connection"
                   and value.get("completion_permission") is False,
                   "timed prompt receipt is native agent_timer data without completion authority")
        return value

    def read_prompt(self, timer_id):
        deadline = time.monotonic() + 5
        while True:
            value = self.prompt(action="read", timer_id=timer_id, limit=4)
            if value.get("events"):
                break
            status = self.prompt(action="status", timer_id=timer_id)
            visibility = status.get("schedule", {}).get("visibility")
            self.record("prompt_visibility_observed", timer_id=timer_id,
                        visibility=visibility, visibility_deferred=value.get("visibility_deferred"))
            if (value.get("visibility_deferred") != 1
                    or visibility not in {"queued", "in_progress", "recorded"}
                    or time.monotonic() >= deadline):
                raise AssertionError(
                    f"original timer {timer_id} has no recorded visible event: {visibility}; "
                    "no schedule, publication, or ACK was replayed")
            time.sleep(0.05)
        self.check(value.get("ack_required") is True and len(value.get("events", [])) == 1,
                   "one timer retains one explicitly acknowledged pending event")
        event = value["events"][0]
        self.check(event.get("timer_id") == timer_id and event.get("provenance") == "agent_timer"
                   and event.get("completion_permission") is False,
                   "native prompt event keeps its timer identity and source")
        return event

    def ack_prompt(self, event):
        return self.prompt(action="ack", timer_id=event["timer_id"],
                           event_id=event["event_id"], revision=event["revision"])

    def visible_prompt(self, event):
        """Read a finite original-thread journal snapshot; never publish or ACK."""
        cursor, generation, end = None, None, None
        rows = []
        for _ in range(32):
            arguments = {"action": "thread", "client_id": self.client_id,
                         "agent_id": self.member["agent_id"], "limit": 8}
            if cursor is not None:
                arguments.update(cursor=cursor, generation=generation)
            page = self.native("ssmcp.ui.events", arguments)
            if generation is None:
                generation, end = page["generation"], page["end_cursor"]
            if (page["generation"] != generation or page.get("retention_gap_possible")
                    or not isinstance(page.get("events"), list)):
                raise AssertionError("original Input journal changed generation or lost retained evidence")
            if page.get("scope", {}).get("client_id") != self.client_id or (
                    page.get("scope", {}).get("agent_id") != self.member["agent_id"]):
                raise AssertionError("native Input journal returned another caller scope")
            self.check(all(not item.get("user_messages") for item in mappings(self.last_public_reply)),
                       "visible Input observation does not create or claim human input")
            rows.extend(row for row in page["events"] if row["sequence"] < end)
            next_cursor = page["next_cursor"]
            if next_cursor >= end:
                break
            if cursor is not None and next_cursor <= cursor:
                raise AssertionError("native Input journal cursor failed to advance")
            cursor = next_cursor
        else:
            raise AssertionError("original Input journal exceeded bounded finite-page observation")
        matches = [row for row in rows if isinstance(row.get("value"), dict)
                   and row["value"].get("type") == "ssmcp.message"
                   and row["value"].get("kind") == "input"
                   and row["value"].get("input_origin") == "agent_timer"
                   and row["value"].get("text") == event["body"]]
        self.check(len(matches) == 1,
                   "the original timer event revision has exactly one native visible Input record")
        row = matches[0]
        value, source = row["value"], row["source"]
        self.check(row.get("kind") == "input" and row.get("input_side") is True
                   and row.get("input_origin") == "agent_timer"
                   and source.get("kind") == "thread"
                   and source.get("client_id") == self.client_id
                   and source.get("agent_id") == self.member["agent_id"]
                   and value.get("role") == "tool" and value.get("completion_permission") is False
                   and isinstance(value.get("message_id"), str)
                   and re.fullmatch(r"[A-Za-z0-9_-]{1,128}", value["message_id"]) is not None
                   and row.get("message_id") == value["message_id"]
                   and row.get("truncated") is False,
                   "visible timer Input retains its native ID, original caller and passive provenance")
        return {"timer_id": event["timer_id"], "event_id": event["event_id"],
                "revision": event["revision"], "generation": generation, "record": row}

    def timed_case(self):
        if "ssmcp.time.prompt" not in self.native_tools:
            if self.options.require_timed_prompts:
                raise CapabilityUnavailable("matching native timed-prompt capability is required")
            self.checkpoint(timed_prompt_case={"status": "not_advertised", "executed": False})
            return
        descriptor = self.native_tools["ssmcp.time.prompt"]
        actions = descriptor.get("inputSchema", {}).get("properties", {}).get("action", {}).get("enum", [])
        self.check({"schedule", "read", "ack", "cancel", "status", "list"} <= set(actions),
                   "native timed-prompt schema advertises all exercised operations")
        label = "PROMPT_" + uuid.uuid4().hex
        once = self.prompt(action="schedule", after_ms=1000, lifetime_ms=10000,
                           request_id="once_" + uuid.uuid4().hex,
                           template="label={{label}} tick={{tick}} scheduled={{scheduled_at}} fired={{fired_at}}",
                           variables={"label": label})
        once_id = once["schedule"]["id"]
        self.timer_ids.append(once_id)
        repeat = self.prompt(action="schedule", after_ms=1000, interval_ms=1000,
                             lifetime_ms=30000, max_fires=20, request_id="repeat_" + uuid.uuid4().hex,
                             template="label={{label}} timer={{timer_id}} tick={{tick}} missed={{missed}}",
                             variables={"label": label})
        repeat_id = repeat["schedule"]["id"]
        self.timer_ids.append(repeat_id)
        self.record("timers_scheduled", once=once, recurring=repeat)
        self.no_http_until(time.monotonic() + 2.3, "timed prompts queue without an HTTP caller")
        code = "await collab.bind(%s, %s); return null;" % (
            json.dumps(self.client_id),
            json.dumps({"agent_id": self.member["agent_id"], "token": self.member["token"]}))
        # Observe the original schedules while the native publisher owns a
        # pending write. Reading never schedules, republishes, or acknowledges it.
        deadline = time.monotonic() + 5
        observed = {}
        while set(observed) != set(self.timer_ids):
            outer = self.tool("code_mode", {"code": code, "timeout_ms": 10000})
            for hook in outer.get("prompt_hooks", []):
                event = hook.get("event") if isinstance(hook, dict) else None
                if isinstance(event, dict) and event.get("timer_id") in self.timer_ids:
                    observed[event["timer_id"]] = hook
            self.check(all(not item.get("user_messages") for item in mappings(self.last_public_reply)),
                       "each timed-hook observation remains separate from human user_messages")
            if set(observed) == set(self.timer_ids):
                break
            if time.monotonic() >= deadline:
                states = {timer_id: self.prompt(action="status", timer_id=timer_id)
                          for timer_id in self.timer_ids if timer_id not in observed}
                raise AssertionError(
                    f"original timed hooks remain unavailable: {states}; no schedule was replayed")
            time.sleep(0.05)
        matching = [observed[timer_id] for timer_id in self.timer_ids]
        self.check({hook["event"]["timer_id"] for hook in matching} == set(self.timer_ids),
                   "bound HTTP observations deliver both original queued timed prompt hooks")
        self.check(all(hook.get("provenance") == "agent_timer"
                       and hook.get("scope") == "native_connection"
                       and hook.get("client_id") == self.client_id
                       and hook.get("agent_id") == self.member["agent_id"]
                       and hook["event"].get("completion_permission") is False for hook in matching),
                   "outer prompt hooks retain original native session and agent attribution")
        self.check(all(not item.get("user_messages") for item in mappings(self.last_public_reply)),
                   "timed hooks are separate from human user_messages")
        once_event = self.read_prompt(once_id)
        self.check(once_event["tick"] == 1 and label in once_event["body"]
                   and "{{label}}" not in once_event["body"],
                   "one-shot timer renders scalar variables and fires once")
        once_visible = self.visible_prompt(once_event)
        once_ack = self.ack_prompt(once_event)
        self.check(once_ack.get("acknowledged") is True and once_ack.get("duplicate") is False
                   and once_ack["schedule"]["pending_count"] == 0,
                   "exact one-shot event revision ACK releases its pending body")
        once_ack_again = self.ack_prompt(once_event)
        self.check(once_ack_again.get("acknowledged") is True and once_ack_again.get("duplicate") is True,
                   "same one-shot ACK is idempotent")

        once_after_ack = self.visible_prompt(once_event)
        self.check(once_after_ack == once_visible,
                   "reading and repeated ACK do not republish the one-shot Input record")
        previous = self.read_prompt(repeat_id)
        previous_visible = self.visible_prompt(previous)
        self.no_http_until(time.monotonic() + 2.3, "recurring prompt coalesces while HTTP is absent")
        cancelled = self.prompt(action="cancel", timer_id=repeat_id)
        latest = self.read_prompt(repeat_id)
        self.check(latest["event_id"] == previous["event_id"] and latest["revision"] > previous["revision"]
                   and latest["tick"] > previous["tick"] and latest["missed"] >= previous["missed"],
                   "missed recurring ticks coalesce into one event with a newer revision")
        latest_visible = self.visible_prompt(latest)
        self.check(latest_visible["record"]["message_id"] != previous_visible["record"]["message_id"],
                   "a newer coalesced timer revision has its own native Input identity")
        stale_outer = self.native_reply("ssmcp.time.prompt", {
            "action": "ack", "timer_id": repeat_id, "event_id": previous["event_id"],
            "revision": previous["revision"]})
        stale = stale_outer.get("result", {})
        self.check(stale.get("isError") is True and "stale_prompt_ack" in json.dumps(stale),
                   "stale revision ACK is explicitly rejected")
        retained = self.read_prompt(repeat_id)
        self.check(retained == latest, "stale ACK preserves the complete newer pending revision")
        exact = self.ack_prompt(latest)
        self.check(exact.get("acknowledged") is True and exact.get("duplicate") is False
                   and exact["schedule"]["pending_count"] == 0,
                   "exact latest revision ACK releases the retained recurring event")
        exact_again = self.ack_prompt(latest)
        self.check(exact_again.get("duplicate") is True and exact_again["schedule"]["pending_count"] == 0,
                   "repeated exact ACK remains idempotent after timer cancellation")
        final = self.prompt(action="read", timer_id=repeat_id, limit=4)
        self.check(final.get("events") == [], "cancelled and acknowledged timer has no pending body")
        latest_after_ack = self.visible_prompt(latest)
        self.check(latest_after_ack == latest_visible,
                   "stale and repeated exact ACKs do not republish the latest Input revision")
        self.checkpoint(timed_prompt_case={"status": "passed", "executed": True,
            "visible_input_records": [once_visible, previous_visible, latest_visible],
            "once_id": once_id, "recurring_id": repeat_id, "outer_prompt_hooks": matching,
            "once_event": once_event, "previous_revision": previous, "latest_revision": latest,
            "stale_ack_result": stale, "exact_ack": exact, "cancel_receipt": cancelled,
            "session_restart_persistence": "not exercised or claimed"})

    def run_acceptance(self):
        self.prepare_account()
        if self.options.require_timed_prompts and "ssmcp.time.prompt" not in self.native_tools:
            raise CapabilityUnavailable("required timed-prompt tool is absent; no exec was started")
        self.durable_case()
        self.timed_case()
        self.check(self.external_invocations("client") == [],
                   "owned model canaries recorded no external agent invocation")
        jobs = self.native("ssmcp.exec.list")["jobs"]
        self.check(self.exec_dispatches == 1 and len(jobs) == 2
                   and {job["job_id"] for job in jobs} == {"main", self.job_id}
                   and next(job["pid"] for job in jobs if job["job_id"] == "main")
                   == self.baseline_main_pid,
                   "entire acceptance performed exactly one native exec and started no model")

    def finish(self):
        cleanup = {"native_wait_cancel": None, "timer_cancel": [], "owned_processes": [],
                   "original_execution": None}
        if self.member is not None and self.client is not None and self.client.poll() is None:
            if self.wait_id is not None and not self.wait_acked:
                try:
                    cleanup["native_wait_cancel"] = self.await_call(op="cancel", wait_id=self.wait_id)
                except Exception as error:
                    cleanup["native_wait_cancel"] = {"outcome": "unknown", "error": str(error)}
            if self.job_id is not None:
                try:
                    observed = self.native("ssmcp.exec.read", {"job_id": self.job_id, "wait_ms": 0,
                                           "stdout_cursor": 0, "stderr_cursor": 0, "max_bytes": 32768})
                    execution = {"job_id": self.job_id, "initial_observation": observed}
                    if observed.get("running") is True:
                        execution["cancel_receipt"] = self.native("ssmcp.exec.cancel",
                                                                  {"job_id": self.job_id})
                        deadline = time.monotonic() + 5
                        while True:
                            observed = self.native("ssmcp.exec.read", {"job_id": self.job_id,
                                "wait_ms": 0, "stdout_cursor": 0, "stderr_cursor": 0, "max_bytes": 32768})
                            if observed.get("running") is False or time.monotonic() >= deadline:
                                break
                            time.sleep(0.05)
                    execution["final_observation"] = observed
                    execution["outcome"] = ("terminal_observed" if observed.get("running") is False
                        and observed.get("stdout_eof") is True and observed.get("stderr_eof") is True
                        else "unknown")
                    cleanup["original_execution"] = execution
                    if execution["outcome"] == "unknown":
                        self.report["status"] = "failed"
                except Exception as error:
                    cleanup["original_execution"] = {"job_id": self.job_id,
                        "outcome": "unknown", "error": str(error), "replayed": False}
                    self.report["status"] = "failed"
            elif self.exec_dispatches:
                cleanup["original_execution"] = {"outcome": "unknown",
                    "reason": "original exec dispatch has no verified receipt", "replayed": False}
            if "ssmcp.time.prompt" in self.native_tools:
                for timer_id in self.timer_ids:
                    try:
                        cleanup["timer_cancel"].append(self.native(
                            "ssmcp.time.prompt", {"action": "cancel", "timer_id": timer_id}))
                    except Exception as error:
                        cleanup["timer_cancel"].append({"timer_id": timer_id,
                                                        "outcome": "unknown", "error": str(error)})
        try:
            self.cleanup()
        except Exception as error:
            cleanup["error"] = str(error)
            self.report["status"] = "failed"
            # A cleanup failure must not prevent attempts to reap other owned
            # processes. Never signal a process that this harness did not spawn.
            for process in reversed(self.processes):
                if process.poll() is None:
                    try:
                        self.stop(process)
                    except Exception as remaining_error:
                        cleanup.setdefault("remaining_errors", []).append(str(remaining_error))
            for _, log in self.logs:
                if not log.closed:
                    log.close()
            if self.account_gateway.process is not None:
                self.account_gateway.exit_code = self.account_gateway.process.poll()
        cleanup["fixture_exit_code"] = self.account_gateway.exit_code
        for process in self.processes:
            cleanup["owned_processes"].append({"pid": process.pid, "returncode": process.poll(),
                                                "reaped": process.returncode is not None})
        if any(row["returncode"] is None for row in cleanup["owned_processes"]):
            self.report["status"] = "failed"
            cleanup["error"] = "an owned fixture process remains alive"
        self.report["cleanup"] = cleanup
        log_files = []
        for path, _ in self.logs:
            if path.is_file():
                # The fixture directory is private and deleted. Only scrubbed
                # text, never its auth files or raw stdout credentials, survives.
                text = path.read_text(errors="replace")
                for secret in sorted((s for s in [self.secret, *self.sensitive] if s),
                                     key=len, reverse=True):
                    text = text.replace(secret, "[redacted]")
                target = self.options.evidence / (path.stem + ".redacted.log")
                target.write_text(text, encoding="utf-8")
                target.chmod(0o600)
                log_files.append(file_identity(target))
        self.report["logs"] = log_files


def source_identity(commit, require_timers):
    repository = Path(__file__).resolve().parent.parent
    result = subprocess.run(["git", "--no-optional-locks", "rev-parse", commit + "^{commit}"],
                            cwd=repository, capture_output=True, text=True, timeout=15, check=True)
    if result.stdout.strip() != commit:
        raise ValueError("--build-source-commit must be an exact commit ID")
    tree = subprocess.run(["git", "--no-optional-locks", "rev-parse", commit + "^{tree}"],
                          cwd=repository, capture_output=True, text=True, timeout=15, check=True)
    paths = [
        "Cargo.toml", "Cargo.lock", "src/http/await_tool.rs", "src/http/await_tool/durable.rs",
        "src/http/relay_composer_fixture.rs", "src/wire/wait_display.rs", "src/client.rs",
        "src/runtime/time.rs", "src/runtime.rs", "src/http.rs", "src/broker.rs", "src/code_mode.rs",
    ]
    optional = ["src/runtime/time/prompts.rs", "src/http/prompt_hooks.rs"]
    records = []
    for name in paths + optional:
        result = subprocess.run(["git", "--no-optional-locks", "show", commit + ":" + name],
                                cwd=repository, capture_output=True, timeout=15)
        if result.returncode:
            if name in optional and not require_timers:
                records.append({"path": name, "present_at_build_commit": False})
                continue
            raise ValueError("declared build source omits required implementation: " + name)
        data = result.stdout
        records.append({"path": name, "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()})
    dependencies = [Path(__file__), repository / "scripts/e2e.py",
                    repository / "scripts/e2e_account_gateway.py"]
    return {"declared_binary_build_commit": commit, "declared_binary_build_tree": tree.stdout.strip(),
            "application_sources_at_build_commit": records,
            "executed_script_dependencies": [file_identity(path) for path in dependencies],
            "provenance_note": "Binary/source mapping is the caller's pinned build receipt; hashes are verified locally."}


def parse_args():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--client-binary", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    parser.add_argument("--client-sha256", required=True, help="expected hash from the matching build receipt")
    parser.add_argument("--fixture-sha256", required=True, help="expected hash from the matching build receipt")
    parser.add_argument("--build-source-commit", required=True, help="exact source commit from that build receipt")
    parser.add_argument("--duration-seconds", type=int, default=600,
                        help="15..1200; values other than 600 are explicitly labeled diagnostic")
    parser.add_argument("--require-timed-prompts", action="store_true",
                        help="fail before exec if the matching native timed-prompt tool is absent")
    parser.add_argument("--report", type=Path, required=True,
                        help="new JSON report path; an existing path prevents accidental reruns")
    options = parser.parse_args()
    if not 15 <= options.duration_seconds <= 1200:
        parser.error("--duration-seconds must be in 15..1200")
    if not re.fullmatch(r"[0-9a-f]{40}", options.build_source_commit):
        parser.error("--build-source-commit must contain exactly 40 lowercase hexadecimal digits")
    for key in ("client_sha256", "fixture_sha256"):
        if not re.fullmatch(r"[0-9a-f]{64}", getattr(options, key)):
            parser.error("--" + key.replace("_", "-") + " must be an exact SHA256")
    for key in ("client_binary", "fixture_binary"):
        path = getattr(options, key).resolve(strict=True)
        if not path.is_file() or not os.access(path, os.X_OK):
            parser.error("prebuilt executable unavailable: " + str(path))
        setattr(options, key, path)
    options.report = options.report.resolve()
    options.evidence = options.report.with_suffix(".evidence")
    return options


def main():
    options = parse_args()
    options.report.parent.mkdir(parents=True, exist_ok=True)
    # A previous or uncertain run is never silently overwritten or replayed.
    with options.report.open("x", encoding="utf-8") as stream:
        os.chmod(options.report, 0o600)
        stream.write('{"status":"reserved","exec_dispatches":0}\n')
    options.evidence.mkdir(mode=0o700)
    began = time.monotonic()
    report = {"schema_version": 1, "status": "failed", "platform": sys.platform,
        "duration_seconds_requested": options.duration_seconds,
        "acceptance_mode": "600_second" if options.duration_seconds == 600 else "short_diagnostic",
        "transport": "production account router + actual OAuth + native WebSocket session on owned loopback",
        "real_user_account": False, "models_started": False, "tls_verified": False,
        "native_session_restarted": False, "restart_persistence_claimed": False,
        "fixture_timeout_seconds": min(1800, options.duration_seconds + 180)}
    harness = None
    prior_handlers = {}

    def interrupted(number, _frame):
        raise InterruptedError(f"received signal {number}; preserve uncertain outcomes and clean owned processes")

    try:
        for number in (signal.SIGINT, signal.SIGTERM):
            prior_handlers[number] = signal.signal(number, interrupted)
        binaries = {"client": file_identity(options.client_binary),
                    "router_fixture": file_identity(options.fixture_binary),
                    "native_command_interpreter": file_identity(sys.executable),
                    "native_session_shell": file_identity("/bin/sh")}
        report["binaries"] = binaries
        if binaries["client"]["sha256"] != options.client_sha256:
            raise ValueError("client hash differs from the supplied matching build receipt")
        if binaries["router_fixture"]["sha256"] != options.fixture_sha256:
            raise ValueError("router fixture hash differs from the supplied matching build receipt")
        report["sources"] = source_identity(options.build_source_commit, options.require_timed_prompts)
        atomic_json(options.report, report)
        with tempfile.TemporaryDirectory(prefix="ssmcp-durable-wait-") as directory:
            harness = DurableWait(Path(directory), options, report)
            try:
                harness.run_acceptance()
                report["status"] = "passed"
            except CapabilityUnavailable as error:
                report["status"] = "blocked"
                report["error"] = str(error)
            except Exception as error:
                report["status"] = "failed"
                report["error"] = f"{type(error).__name__}: {error}"
            finally:
                harness.finish()
                report["checks"] = harness.checks
                report["exec_dispatches"] = harness.exec_dispatches
                report["http_requests"] = harness.http_requests
                report["original_job_id"] = harness.job_id
                report["original_wait_id"] = harness.wait_id
    except Exception as error:
        report["status"] = "failed"
        report["error"] = f"{type(error).__name__}: {error}"
    finally:
        for number, handler in prior_handlers.items():
            signal.signal(number, handler)
        report["duration_seconds_actual"] = round(time.monotonic() - began, 3)
        secrets = [harness.secret, *harness.sensitive] if harness else []
        report = redact(report, secrets)
        evidence = []
        for path in sorted(options.evidence.iterdir()):
            if path.is_file():
                evidence.append(file_identity(path))
        report["evidence"] = evidence
        atomic_json(options.report, report)
        print(json.dumps({"status": report["status"], "report": str(options.report),
                          "sha256": digest(options.report), "checks": len(report.get("checks", [])),
                          "exec_dispatches": report.get("exec_dispatches", 0)}, ensure_ascii=False), flush=True)
    return 0 if report["status"] == "passed" else 2 if report["status"] == "blocked" else 1


if __name__ == "__main__":
    raise SystemExit(main())
