#!/usr/bin/env python3
"""Validate the unified client through real local account WebSocket, OAuth and MCP.

Reuses e2e.py's isolated account WebSocket/PKCE harness and the session helper's
reload reader and output pagination checks. No installed Harness is exposed to
the client PATH, no model is invoked, and no package or runtime is installed.
Only generated fixtures and the test's own output artifacts are changed.

Example: python3 scripts/e2e_unified.py --bin-dir target/debug --timeout 240 \
    --report target/acceptance/unified.json --revision <tested-commit>
"""

from __future__ import annotations

import argparse
import base64
from collections import defaultdict
from contextlib import ExitStack
import fcntl
import hashlib
import json
import math
import os
from pathlib import Path
import signal
import sys
import tempfile
import time

from e2e import wait_for
from e2e_output_expiry import OutputExpiryFailure, run as verify_output_expiry
from e2e_session_features import OUTPUT_RETAINED_BYTES, OUTPUT_RETENTION_SECS, SessionFeatures


TITLE = "统一能力验收：独立命令、事件、审计与生命周期"
LONG_BYTES = 160 * 1024 + 17  # Exceeds both inline storage and the executor's live tail.
LONG_CHUNK_BYTES = 32 * 1024  # Each unacknowledged event burst fits the bounded ring.
LONG_PREFIX = b"UNIFIED_LONG_BEGIN\n"
LONG_LIVE = b"\nUNIFIED_LONG_AFTER_SPILL\n"
LONG_DONE = b"UNIFIED_LONG_DONE\n"

BARRIER_PROGRAM = r'''
import hashlib, json, os, sys, time
from pathlib import Path
name, other = sys.argv[1:]
pid = os.getpid()
Path(name + '.ready').write_text(json.dumps({
    'command_pid': pid, 'parent_pid': os.getppid(), 'cwd': os.getcwd(),
    'env_sha256': hashlib.sha256(os.environ['UNIFIED_PROBE'].encode()).hexdigest()}))
deadline = time.monotonic() + 40
while not Path(other + '.ready').exists():
    if time.monotonic() > deadline: raise RuntimeError('independent peer never started')
    time.sleep(.02)
print('UNIFIED_%s_OUT_%s' % (name, pid), flush=True)
print('UNIFIED_%s_ERR_%s' % (name, pid), file=sys.stderr, flush=True)
while not Path('concurrent.release').exists():
    if time.monotonic() > deadline: raise RuntimeError('test did not release commands')
    time.sleep(.02)
print('UNIFIED_%s_DONE_%s' % (name, pid), flush=True)
sys.exit(0 if name == 'alpha' else 7)
'''

LONG_PROGRAM = r'''
import fcntl, sys, time
from pathlib import Path
payload = (b'UNIFIED_LONG_BEGIN\n' + b'Z' * int(sys.argv[1])
           + b'\nUNIFIED_LONG_AFTER_SPILL\n')
ack_directory, chunk_bytes = Path(sys.argv[2]), int(sys.argv[3])
for start in range(0, len(payload), chunk_bytes):
    end = min(start + chunk_bytes, len(payload))
    sys.stdout.buffer.write(payload[start:end])
    sys.stdout.flush()
    # The consumer owns this lock until it verifies every event byte in the chunk.
    # ACK files are private to this fixture; stdin keeps its original null mode.
    with (ack_directory / f'{end}.ack').open('rb') as ack:
        fcntl.flock(ack, fcntl.LOCK_SH)
        if ack.read() != f'{end}\n'.encode():
            raise RuntimeError('consumer did not acknowledge the exact output offset')
print('UNIFIED_LONG_STDERR', file=sys.stderr, flush=True)
Path('long.ready').write_text('ready')
deadline = time.monotonic() + 60
while not Path('long.release').exists():
    if time.monotonic() > deadline: raise RuntimeError('test did not release output command')
    time.sleep(.02)
print('UNIFIED_LONG_DONE', flush=True)
'''

TREE_PROGRAM = r'''
import json, os, sys, time
from pathlib import Path
command_pid = os.getpid()
read, write = os.pipe()
first = os.fork()
if first == 0:
    os.close(read)
    os.setsid()
    if os.fork() != 0:
        os._exit(0)
    Path(sys.argv[1]).write_text(json.dumps({
        'command_pid': command_pid, 'daemon_pid': os.getpid()}))
    os.write(write, b'R')
    os.close(write)
    # Keep inherited stdout/stderr open: only complete descendant cleanup can
    # make the command's captured output reach EOF after its native parent exits.
    time.sleep(120)
    os._exit(0)
os.close(write)
if os.read(read, 1) != b'R':
    raise RuntimeError('detached descendant did not start')
os.close(read)
os.waitpid(first, 0)
print('UNIFIED_TREE_READY', flush=True)
deadline = time.monotonic() + 120
while not Path(sys.argv[1] + '.release').exists():
    if time.monotonic() > deadline:
        raise RuntimeError('fixture owner did not complete the lifecycle check')
    time.sleep(.02)
sys.exit(17)
'''


def fixture_json(path):
    try:
        return json.loads(path.read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return None


def process_identity(pid):
    """Bind cleanup assertions to a Linux process instance, not a reusable PID."""
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return {"pid": pid, "start_ticks": int(fields[19]), "state": fields[0],
                "parent_pid": int(fields[1]), "session_pid": int(fields[3])}
    except FileNotFoundError:
        return None


def still_present(identity):
    current = process_identity(identity["pid"])
    return bool(current and current["start_ticks"] == identity["start_ticks"])


def still_running(identity):
    current = process_identity(identity["pid"])
    return bool(current and current["start_ticks"] == identity["start_ticks"]
                and current["state"] != "Z")


class Unified(SessionFeatures):
    def __init__(self, directory, binary_dir, evidence_dir):
        super().__init__(directory, binary_dir)
        self.evidence_dir = evidence_dir
        self.evidence_dir.mkdir(parents=True, exist_ok=True, mode=0o700)
        self.sections = {}
        self.current_section = "startup"
        self.evidence = []
        self.output_expiry_evidence = None
        self.output_expiry_record = None
        self.output_expiry_observed = False
        self.fixture_timeout_seconds = 240
        self.exec_cursor = 0
        self.exec_events = []
        self.streams = defaultdict(lambda: {"stdout": bytearray(), "stderr": bytearray()})
        self.tree_identities = []
        self.python = str(Path(sys.executable).resolve())
        self.env_marker = "unified-fixture-" + os.urandom(16).hex()
        self.sensitive.append(self.env_marker)

    def scrub(self, text):
        for secret in [self.secret, *self.sensitive]:
            if secret:
                text = text.replace(secret, "[redacted]")
        return text

    def save(self, name, value, *, jsonl=False):
        path = self.evidence_dir / name
        if isinstance(value, bytes):
            data = value
        elif jsonl:
            data = "".join(json.dumps(row, ensure_ascii=False) + "\n" for row in value).encode()
        else:
            data = (json.dumps(value, ensure_ascii=False, indent=2) + "\n").encode()
        # Evidence contains only generated fixtures, bounded protocol records and
        # redacted client logs. Credentials are never persisted.
        if not isinstance(value, bytes):
            data = self.scrub(data.decode()).encode()
        path.write_bytes(data)
        path.chmod(0o600)
        item = {"path": str(path.resolve()), "bytes": len(data),
                "sha256": hashlib.sha256(data).hexdigest()}
        self.evidence = [entry for entry in self.evidence if entry["path"] != item["path"]]
        self.evidence.append(item)
        return item

    def track(self, value, *, client_id=None):
        if isinstance(value, dict):
            if value.get("schema") == "ssmcp.output.v1":
                self.artifact_metadata(value, client_id=client_id)
            for child in value.values():
                self.track(child, client_id=client_id)
        elif isinstance(value, list):
            for child in value:
                self.track(child, client_id=client_id)

    def call(self, name, arguments=None, **kwargs):
        value = super().call(name, arguments, **kwargs)
        self.track(value, client_id=kwargs.get("client_id"))
        if isinstance(value, dict) and value.get("spilled") and value.get("format") == "mcp_tool_result_json":
            original = json.loads(self.artifact_bytes(value["artifact"], client_id=kwargs.get("client_id"), secret=kwargs.get("secret")))
            if original.get("isError"):
                raise AssertionError(f"Archived native tool {name} failed: {original}")
            value = original["structuredContent"]
            self.track(value, client_id=kwargs.get("client_id"))
        return value

    def code(self, source, **kwargs):
        return self.public_call("code_mode", {"code": source}, **kwargs)["result"]

    def tool_catalog(self, *, through_code=False):
        if through_code:
            return self.code("""
                const collected = [], seen = new Set();
                let cursor;
                do {
                    const page = await tools(%s, cursor);
                    collected.push(...page.tools);
                    cursor = page.nextCursor;
                    if (cursor && seen.has(cursor)) throw Error('catalog cursor repeated');
                    if (cursor) seen.add(cursor);
                } while (cursor);
                return collected;
            """ % json.dumps(self.client_id))
        collected, seen, cursor = [], set(), None
        while True:
            arguments = {"client_id": self.client_id}
            if cursor:
                arguments["cursor"] = cursor
            page = self.public_call("list", arguments)
            collected.extend(page["tools"])
            cursor = page.get("nextCursor")
            if not cursor:
                return collected
            if cursor in seen:
                raise AssertionError("list repeated a catalog cursor")
            seen.add(cursor)

    def rejected(self, name, arguments, *, client_id=None, secret=None):
        result = self.code("""
            try {
                const value = await call(%s, %s, %s);
                return {rejected:value.isError === true, value};
            } catch (error) { return {rejected:true, message:String(error)}; }
        """ % (json.dumps(client_id or self.client_id), json.dumps(name), json.dumps(arguments)), secret=secret)
        if not result.get("rejected"):
            raise AssertionError(f"{name} unexpectedly accepted a forbidden/stale operation")
        return result

    def finish_job(self, job, timeout=10):
        def complete():
            state = self.call("ssmcp.exec.wait", {"job_id": job["job_id"], "wait_ms": 500})
            return state if not state["running"] else None
        return wait_for(complete, timeout=timeout, description="final independent command state")

    def events(self, tool, *, cursor=0, client_id=None, secret=None):
        rows, end_cursor = [], None
        # Every call is itself audited. Freeze the first page's stream boundary,
        # or audit polling produces new events faster than an empty page can occur.
        while end_cursor is None or cursor < end_cursor:
            count = 8 if end_cursor is None else min(8, end_cursor - cursor)
            page = self.call(tool, {"cursor": cursor, "max_events": count, "wait_ms": 0}, client_id=client_id, secret=secret)
            if page["truncated"]:
                raise AssertionError(f"{tool} lost a test event before it could be inspected")
            if end_cursor is None:
                end_cursor = page["end_cursor"]
            batch = page["events"]
            for row in batch:
                if row["cursor"] != cursor:
                    raise AssertionError(f"{tool} event cursor skipped or duplicated an event")
                cursor += 1
            if cursor != page["next_cursor"]:
                raise AssertionError(f"{tool} returned an inconsistent next_cursor")
            if not cursor <= end_cursor <= page["end_cursor"] or (not batch and cursor < end_cursor):
                raise AssertionError(f"{tool} returned an inconsistent event snapshot")
            rows.extend(batch)
        return rows, cursor

    def poll_exec(self):
        rows, self.exec_cursor = self.events("ssmcp.exec.events", cursor=self.exec_cursor)
        self.exec_events.extend(rows)
        for row in rows:
            event = row["event"]
            if event.get("type") not in ("exec.stdout", "exec.stderr"):
                continue
            job_id, params = event["job_id"], event["params"]
            stream = params["stream"]
            if event["type"] != "exec." + stream or params["processId"] != job_id:
                raise AssertionError("execution output lost its job or stdout/stderr source")
            chunk = base64.b64decode(params["deltaBase64"], validate=True)
            stored = self.streams[job_id][stream]
            if params["offset"] != len(stored) or params["next_offset"] != len(stored) + len(chunk):
                raise AssertionError("execution event bytes were repeated, skipped or reordered")
            stored.extend(chunk)
        self.save("execution-events.jsonl", self.exec_events, jsonl=True)

    def wait_stream(self, job_id, stream, marker):
        def visible():
            self.poll_exec()
            return marker in self.streams[job_id][stream]
        wait_for(visible, timeout=20, description=f"live {stream} before command completion")

    def bootstrap(self):
        self.prepare()
        self.workspace = self.directory / "unified-workspace"
        self.workspace.mkdir()
        self.client, self.reader, self.original = self.titled_client("unified-primary", TITLE, self.workspace)
        self.oauth_login(self.original)
        self.client_id = self.original["client_id"]
        self.check(self.call("terminal.status")["running"], "a client without any Harness starts its interactive shell")

    def catalog(self):
        for modern in (False, True):
            status, result, _ = self.rpc("tools/list", modern=modern)
            self.check(status == 200 and [t["name"] for t in result["result"]["tools"]] == ["link", "list", "code_mode", "collab", "await", "messages"],
                       f"public MCP exposes exactly link/list/code_mode/collab/await/messages (modern={modern})")
        provider_names = {"ssmcp"}
        discovery = self.call("ssmcp.harnesses.list")
        rows = discovery["harnesses"]
        providers = {row["provider"]: row for row in rows}
        self.check(len(rows) == len(provider_names) and set(providers) == provider_names,
                   "all optional providers have independent lazy state records")
        self.check(all(row["status"] == "not_loaded" and row["tools"] == 0
                       and "executable" in row and row["executable"] is None
                       and "version" in row and row["version"] is None for row in rows),
                   "native startup lists optional providers without probing executables or versions")
        self.check(self.external_invocations("unified-primary") == [],
                   "native startup and observation execute no external fixture programs")
        inventory = self.public_call("list")
        selected = next(client for client in inventory["clients"] if client["id"] == self.client_id)
        self.check(selected["title"] == TITLE and selected["cwd"] == str(self.workspace.resolve()),
                   "list preserves the connection title and authorized workspace")
        listed = self.tool_catalog()
        invoked = self.tool_catalog(through_code=True)
        # Compare full descriptors, including complete JSON Schemas/extensions.
        index = lambda tools: {tool["name"]: tool for tool in tools}
        self.check(index(listed) == index(invoked) and len(index(listed)) == len(listed)
                   and len(index(invoked)) == len(invoked),
                   "list and code_mode tools return one identical collision-free full-schema catalog")
        self.check(not any(t["name"].startswith(("codex.", "claude.", "pi.", "opencode.", "playwright.", "peer.")) for t in listed),
                   "unloaded optional providers contribute no invented native tools")
        self.initial_catalog = index(listed)
        self.save("catalog.json", {"client": selected, "tools": listed, "discovery": discovery})


    def record_job_processes(self, job, command_pid):
        supervisor = process_identity(job["pid"])
        command = process_identity(command_pid)
        identities = []
        for role, identity in (("supervisor", supervisor), ("command", command)):
            if identity:
                identity = {**identity, "role": role, "job_id": job["job_id"]}
                self.tree_identities.append(identity)
                identities.append(identity)
        self.check(job["pid_role"] == "supervisor" and supervisor and command
                   and still_running(supervisor) and still_running(command)
                   and command_pid != job["pid"] and command["parent_pid"] == job["pid"],
                   "reported supervisor PID owns the separately observed live command PID")
        return identities

    def concurrent_commands(self):
        arguments = [{"command": [self.python, "-u", "-c", BARRIER_PROGRAM, name, other],
                      "env": {"UNIFIED_PROBE": self.env_marker}}
                     for name, other in (("alpha", "beta"), ("beta", "alpha"))]
        jobs = self.code("return await Promise.all(%s.map(async args => { const r=await call(%s, 'ssmcp.exec.start', args); if(r.isError) throw Error(JSON.stringify(r)); return r.structuredContent; }));" %
                         (json.dumps(arguments), json.dumps(self.client_id)))
        self.check(len({job["job_id"] for job in jobs}) == 2 and len({job["pid"] for job in jobs}) == 2
                   and all(job["running"] and job["exit_code"] is None and job["exit_signal"] is None
                           and job["pid_role"] == "supervisor" for job in jobs),
                   "two starts return distinct active processes without claiming command success")
        self.concurrent_jobs = jobs
        observations = []
        for name, job in zip(("alpha", "beta"), jobs):
            observed = wait_for(lambda: fixture_json(self.workspace / f"{name}.ready"),
                                timeout=10, description="both command barrier participants")
            self.record_job_processes(job, observed["command_pid"])
            self.check(observed["parent_pid"] == job["pid"] and observed["cwd"] == str(self.workspace.resolve())
                       and observed["env_sha256"] == hashlib.sha256(self.env_marker.encode()).hexdigest(),
                       f"{name} runs in the authorized cwd with its explicit environment")
            for stream, label in (("stdout", "OUT"), ("stderr", "ERR")):
                marker = f"UNIFIED_{name}_{label}_{observed['command_pid']}".encode()
                self.wait_stream(job["job_id"], stream, marker)
            self.check(self.call("ssmcp.exec.status", {"job_id": job["job_id"]})["running"],
                       f"{name} emits live stdout/stderr while blocked behind the release gate")
            observations.append(observed)
        # The interactive shell is independently available while both jobs wait.
        self.quiet_terminal(self.client_id)
        self.call("terminal.write", {"data": "printf '%s%s\\n' UNIFIED_PTY_ INDEPENDENT\n"})
        self.read_until(r"UNIFIED_PTY_INDEPENDENT", timeout=5)
        log = (self.directory / "unified-primary.log").read_text(errors="replace")
        self.check(all(log.count(f"UNIFIED_{name}_{label}_{observed['command_pid']}") == 1
                       for name, observed in zip(("alpha", "beta"), observations) for label in ("OUT", "ERR")),
                   "the terminal displays each live job/stdout/stderr marker once before any wait completes")
        self.call("ssmcp.fs.write_text", {"path": "concurrent.release", "text": "release\n"})
        final = [self.finish_job(job) for job in jobs]
        self.poll_exec()
        self.check([state["exit_code"] for state in final] == [0, 7]
                   and all(not state["running"] for state in final),
                   "final exit codes distinguish a successful command from a started command that exits 7")
        for name, job in zip(("alpha", "beta"), jobs):
            read = self.call("ssmcp.exec.read", {"job_id": job["job_id"]})
            self.check(base64.b64decode(read["stdout_base64"]) == bytes(self.streams[job["job_id"]]["stdout"])
                       and base64.b64decode(read["stderr_base64"]) == bytes(self.streams[job["job_id"]]["stderr"]),
                       f"{name} read-back exactly matches ordered, nonduplicated live events per stream")
        self.check(self.env_marker not in json.dumps(self.exec_events) and self.env_marker not in log,
                   "the environment value is absent from audit events and the terminal transcript")
        rejected = self.rejected("ssmcp.exec.start", {"command": [self.python, "-c", "pass"], "cwd": str(self.directory)})
        self.save("concurrent-commands.json", {"started": jobs, "observed": observations, "final": final,
                                               "outside_workspace_rejected": rejected})

    def utf8_transport(self):
        expected = ("a" * 51 + chr(0x3001) + "exit=0").encode("utf-8")
        # Keep the argv ASCII: only the owned command emits the UTF-8 boundary
        # fixture through the real executor, audit, events and WSS result path.
        program = ("import sys; value = ('a' * 51 + chr(0x3001) + 'exit=0').encode('utf-8'); "
                   "sys.stdout.buffer.write(value); sys.stderr.buffer.write(value)")
        job = self.call("ssmcp.exec.start", {
            "command": [self.python, "-u", "-c", program], "timeout_ms": 10000,
        })
        final = self.finish_job(job, timeout=15)
        self.check(not final["running"] and final["exit_code"] == 0
                   and final["exit_signal"] is None and final["error"] is None
                   and final["stdout"]["eof"] and final["stderr"]["eof"],
                   "UTF-8 command output completes with exit zero and both streams at EOF")
        read = self.call("ssmcp.exec.read", {"job_id": job["job_id"]})
        observed = {}
        for stream in ("stdout", "stderr"):
            observed[stream] = base64.b64decode(read[stream + "_base64"], validate=True)
            self.check(observed[stream] == expected,
                       f"UTF-8 {stream} preserves the exact byte-52 delimiter fixture")
        self.poll_exec()
        self.check(all(bytes(self.streams[job["job_id"]][stream]) == expected
                       for stream in ("stdout", "stderr")),
                   "UTF-8 stdout and stderr remain exact in ordered audit event bytes")
        terminal = self.call("terminal.status")
        self.check(terminal["running"] and self.client.poll() is None,
                   "a subsequent native call succeeds on the same live connection after UTF-8 output")
        self.save("utf8-transport.json", {
            "job_id": job["job_id"], "exit_code": final["exit_code"],
            "client_id": self.client_id, "followup_terminal_running": terminal["running"],
            "expected_bytes": len(expected), "expected_sha256": hashlib.sha256(expected).hexdigest(),
            "stdout_sha256": hashlib.sha256(observed["stdout"]).hexdigest(),
            "stderr_sha256": hashlib.sha256(observed["stderr"]).hexdigest(),
        })

    def outputs(self):
        payload = LONG_PREFIX + b"Z" * LONG_BYTES + LONG_LIVE
        # A single 160 KiB burst becomes larger than the event ring when both its
        # base64 and decoded text are retained. Drain and verify each bounded
        # chunk before releasing the next one; never hide a truncated event page.
        with tempfile.TemporaryDirectory(prefix="long-output-acks-", dir=self.workspace) as directory, ExitStack() as locks:
            gates = []
            for start in range(0, len(payload), LONG_CHUNK_BYTES):
                end = min(start + LONG_CHUNK_BYTES, len(payload))
                ack = locks.enter_context((Path(directory) / f"{end}.ack").open("x+b"))
                os.fchmod(ack.fileno(), 0o600)
                fcntl.flock(ack, fcntl.LOCK_EX)
                gates.append((end, ack))
            job = self.call("ssmcp.exec.start", {
                "command": [self.python, "-u", "-c", LONG_PROGRAM, str(LONG_BYTES),
                            directory, str(LONG_CHUNK_BYTES)],
            })
            acknowledged = []

            def acknowledge_chunk():
                self.poll_exec()
                end, ack = gates[len(acknowledged)]
                observed = bytes(self.streams[job["job_id"]]["stdout"])
                if len(observed) > end or observed != payload[:len(observed)]:
                    raise AssertionError("long output crossed its ACK gate or changed a verified prefix")
                if len(observed) < end:
                    return False
                ack.write(f"{end}\n".encode())
                ack.flush()
                fcntl.flock(ack, fcntl.LOCK_UN)
                acknowledged.append(end)
                return len(acknowledged) == len(gates)

            wait_for(acknowledge_chunk, timeout=20, description="verified long-output chunks before ACK")
            self.check(acknowledged[-1] == len(payload),
                       "each bounded output chunk is acknowledged only after exact live event verification")
            live = self.call("ssmcp.exec.status", {"job_id": job["job_id"]})
            self.check(live["running"] and live["stdout"]["artifact"] and not live["stdout"]["artifact"]["complete"],
                       "output spilling does not stop live events and active capture remains explicitly incomplete")
            self.call("ssmcp.fs.write_text", {"path": "long.release", "text": "release\n"})
            done = self.finish_job(job)
            self.poll_exec()
        expected = LONG_PREFIX + b"Z" * LONG_BYTES + LONG_LIVE + LONG_DONE
        artifact = done["stdout"]["artifact"]
        raw = self.artifact_bytes(artifact)
        self.check(raw == expected and bytes(self.streams[job["job_id"]]["stdout"]) == expected,
                   "long stdout retains its complete prefix/body/tail in both stored bytes and live events")
        self.check(artifact["complete"] and artifact["bytes"] == len(expected)
                   and artifact["observed_bytes"] == len(expected)
                   and artifact["lines"] == expected.count(b"\n")
                   and artifact["sha256"] == hashlib.sha256(expected).hexdigest()
                   and artifact["retention_secs"] == OUTPUT_RETENTION_SECS and artifact["expires_at_unix"] > time.time(),
                   "long-output metadata truthfully reports full integrity, SHA-256, lines and 900-second memory retention")
        pages = self.verify_pagination(self.client_id, artifact)
        self.retained_artifact = artifact
        self.save("long-stdout.log", raw)
        self.save("long-output.json", {"live": live, "final": done, "pages": pages,
                                      "chunk_bytes": LONG_CHUNK_BYTES, "acknowledged_offsets": acknowledged})
        line_job = self.call("ssmcp.exec.start", {"command": [self.python, "-c", "import sys; sys.stdout.write('x\\n'*600)"]})
        lines = self.finish_job(line_job)["stdout"]["artifact"]
        self.check(lines["complete"] and lines["lines"] == 600 and lines["bytes"] < 32768,
                   "600 short lines independently trigger the existing line threshold")
        self.expiring_artifact = lines
        self.check(self.artifact_bytes(lines) == b"x\n" * 600,
                   "the line-threshold memory output retains every actual generated byte")
        typed = "typed-é\n" * 5000
        (self.workspace / "typed-output.txt").write_text(typed)
        spilled = super().call("ssmcp.fs.read_text", {"path": "typed-output.txt"})
        self.check(spilled["spilled"] and spilled["format"] == "mcp_tool_result_json",
                   "a large structured result is archived with its native result format")
        native = json.loads(self.artifact_bytes(spilled["artifact"]))
        self.check(native["isError"] is False and native["structuredContent"]["text"] == typed
                   and native["structuredContent"]["eof"] is True
                   and json.loads(native["content"][0]["text"]) == native["structuredContent"],
                   "retrieval preserves the complete structured result and native MCP fields")
        self.save("structured-result.json", {"artifact": spilled["artifact"],
                  "native_keys": sorted(native), "text_sha256": hashlib.sha256(typed.encode()).hexdigest()})
        cleanup = self.call("ssmcp.output.cleanup")
        retained = self.call("ssmcp.output.stat", {"id": artifact["id"]})
        self.check(retained == artifact and self.artifact_bytes(retained) == expected
                   and cleanup["storage"] == "memory" and cleanup["retention_secs"] == OUTPUT_RETENTION_SECS
                   and cleanup["max_retained_bytes"] == OUTPUT_RETAINED_BYTES,
                   "explicit cleanup preserves fresh memory output under the 900-second and 32-MiB policy")

    def workspace_audit(self):
        before, after = "one\nold\nthree\n", "one\nnew\nextra\nthree\n"
        created = self.call("ssmcp.fs.write_text", {"path": "audited.txt", "text": before})
        read = self.call("ssmcp.fs.read_text", {"path": "audited.txt"})
        self.call("ssmcp.fs.edit_text", {"path": "audited.txt", "old_text": "old\n", "new_text": "new\nextra\n",
                                        "expected_sha256": read["sha256"]})
        failure = self.rejected("ssmcp.fs.edit_text", {"path": "audited.txt", "old_text": "new", "new_text": "wrong",
                                                       "expected_sha256": read["sha256"]})
        current = self.call("ssmcp.fs.read_text", {"path": "audited.txt"})
        rows, _ = self.events("ssmcp.fs.events")
        file_events = [row["event"] for row in rows if row["event"].get("path") == "audited.txt"]
        writes = [event for event in file_events if event["type"] == "ssmcp.file.write"]
        diffs = [event for event in file_events if event["type"] == "ssmcp.file.diff"]
        self.check(current["text"] == after and (self.workspace / "audited.txt").read_text() == after
                   and len(writes) == 2 and len(diffs) == 2,
                   "only successful committed writes produce file change events; stale edits leave bytes unchanged")
        edit = diffs[-1]
        self.check(edit["added_lines"] == 2 and edit["removed_lines"] == 1 and edit["diff_complete"]
                   and "-old\n" in edit["diff"] and "+new\n" in edit["diff"] and "+extra\n" in edit["diff"]
                   and any(event["type"] == "ssmcp.file.read" for event in file_events)
                   and edit["external_operations"] == "unknown",
                   "file read and queryable Diff reflect actual bytes without claiming external audit coverage")
        self.call("ssmcp.fs.write_text", {"path": ".env.unified", "text": "API_TOKEN=" + self.env_marker + "\n"})
        self.call("ssmcp.fs.read_text", {"path": ".env.unified"})
        rows, _ = self.events("ssmcp.fs.events")
        protected = [row["event"] for row in rows if row["event"].get("path") == ".env.unified"]
        self.check(self.env_marker not in json.dumps(rows)
                   and all(event.get("diff") is None for event in protected if event["type"] == "ssmcp.file.diff"),
                   "sensitive-file audit records metadata without storing its contents or a diff preview")
        skill_path = "skills/unified-fixture/SKILL.md"
        skill = "---\nname: unified-fixture\ndescription: A local acceptance fixture.\n---\nBODY_ONLY_WHEN_EXPLICITLY_READ\n"
        self.call("ssmcp.fs.write_text", {"path": skill_path, "text": skill, "create_parents": True})
        index = self.call("ssmcp.skills.list")
        discovered, cursor = self.events("ssmcp.audit.events")
        self.check(any(item["name"] == "unified-fixture" for item in index["skills"])
                   and "BODY_ONLY_WHEN_EXPLICITLY_READ" not in json.dumps(index)
                   and any(row["event"]["type"] == "ssmcp.skill.discovered" for row in discovered)
                   and not any(row["event"]["type"] == "ssmcp.skill.read" for row in discovered),
                   "skill discovery emits discovery metadata without claiming that a skill was read or invoked")
        loaded = self.call("ssmcp.skills.read", {"path": skill_path})
        read_events, _ = self.events("ssmcp.audit.events", cursor=cursor)
        self.check(loaded["content"] == skill
                   and any(row["event"]["type"] == "ssmcp.skill.read" and row["event"]["invoked"] is False for row in read_events)
                   and not any(row["event"].get("invoked") is True for row in discovered + read_events),
                   "explicit skill reading is recorded separately and never relabeled as invocation")
        self.save("file-events.jsonl", rows, jsonl=True)
        self.save("skill-events.jsonl", discovered + read_events, jsonl=True)
        self.save("file-results.json", {"created": created, "final": current, "failed_edit": failure, "verified_diff": edit})

    def client_isolation(self):
        sibling_workspace = self.directory / "sibling-workspace"
        sibling_workspace.mkdir()
        self.sibling, _, credentials = self.titled_client("unified-sibling", "隔离验收", sibling_workspace)
        self.sibling_id = credentials["client_id"]
        token, grant = self.secret, self.oauth_grant
        self.oauth_login(credentials)
        self.sibling_token = self.secret
        self.secret, self.oauth_grant = token, grant
        self.check(self.call("terminal.status", client_id=self.sibling_id, secret=self.sibling_token)["running"],
                   "the sibling is authorized and online before testing its per-client resource boundary")
        denied = {}
        for tool in ("ssmcp.output.read", "ssmcp.output.stat"):
            denied[tool] = self.rejected(tool, {"id": self.retained_artifact["id"]}, client_id=self.sibling_id, secret=self.sibling_token)
        denied["job"] = self.rejected("ssmcp.exec.status", {"job_id": self.concurrent_jobs[0]["job_id"]}, client_id=self.sibling_id, secret=self.sibling_token)
        self.check(not self.call("ssmcp.exec.list", client_id=self.sibling_id, secret=self.sibling_token)["jobs"],
                   "a separate client cannot list or address the first client's tasks")
        for tool in ("ssmcp.exec.events", "ssmcp.fs.events", "ssmcp.audit.events"):
            rows, _ = self.events(tool, client_id=self.sibling_id, secret=self.sibling_token)
            isolated = not rows
            if tool == "ssmcp.audit.events":
                self.save("sibling-audit-events.jsonl", rows, jsonl=True)
                # Every code-mode event must still belong to this sibling's run.
                own_runs = {
                    row["event"]["run_id"] for row in rows
                    if row["event"]["type"] == "ssmcp.code_mode"
                    and row["event"].get("phase") == "started"
                    and json.dumps(self.sibling_id) in row["event"].get("code", "")
                    and json.dumps(self.client_id) not in row["event"].get("code", "")
                }
                # This new empty workspace has exactly one startup cleanup event.
                # Match its complete empty result; other lifecycle/data is forbidden.
                empty_startup_cleanup = {
                    "summary": {
                        "busy": False,
                        "decisions": [],
                        "dry_run": False,
                        "enabled": False,
                        "errors": [],
                        "kept_active": 0,
                        "kept_by_rule": 0,
                        "kept_changed": 0,
                        "kept_protected": 0,
                        "missing_files": 0,
                        "recoveries": [],
                        "registry_present": False,
                        "removed_bytes": 0,
                        "removed_files": 0,
                        "retained_temporary_bytes": 0,
                        "retention": {
                            "active_limit_exceeded": False,
                            "active_record_limit": 16384,
                            "active_records": 0,
                            "completed_records": 0,
                            "errors": [],
                            "invalid_records": 0,
                            "pending_completed_records": 0,
                            "prune_limit": 1024,
                            "pruned_records": 0,
                            "pruning_requested": False,
                            "raw_scan_limit": 32768,
                            "raw_scan_truncated": False,
                            "recoveries": [],
                            "retain_completed": 1024,
                            "scanned_entries": 0,
                            "state_changed": False
                        },
                        "scan_truncated": False,
                        "scanned_records": 0,
                        "trigger": "startup",
                        "would_remove_bytes": 0,
                        "would_remove_files": 0
                    },
                    "trigger": "startup",
                    "type": "ssmcp.files.cleanup"
                }
                startup_events = [row["event"] for row in rows
                                  if row["event"]["type"] == "ssmcp.files.cleanup"]
                isolated = (
                    bool(own_runs)
                    and json.dumps(startup_events, sort_keys=True)
                    == json.dumps([empty_startup_cleanup], sort_keys=True)
                    and all(
                        row["event"]["type"] == "ssmcp.files.cleanup"
                        or (row["event"]["type"] == "ssmcp.code_mode"
                            and row["event"].get("run_id") in own_runs)
                        for row in rows
                    )
                )
            self.check(isolated, f"{tool} on another client does not expose the first client's audit stream")
        self.call("ssmcp.output.cleanup", client_id=self.sibling_id, secret=self.sibling_token)
        retained = self.call("ssmcp.output.stat", {"id": self.retained_artifact["id"]})
        self.check(retained == self.retained_artifact
                   and self.artifact_bytes(retained) == LONG_PREFIX + b"Z" * LONG_BYTES + LONG_LIVE + LONG_DONE,
                   "another client's cleanup leaves the first client's retained artifact intact")
        self.save("client-isolation.json", {"primary": self.client_id, "sibling": self.sibling_id, "denied": denied})

    def tree(self, label, *, timeout_ms=0):
        marker = label + ".pids.json"
        job = self.call("ssmcp.exec.start", {"command": [self.python, "-u", "-c", TREE_PROGRAM, marker],
                                              "timeout_ms": timeout_ms})
        observed = wait_for(lambda: fixture_json(self.workspace / marker), timeout=5, description="owned descendant fixture")
        identities = self.record_job_processes(job, observed["command_pid"])
        def adopted():
            daemon = process_identity(observed["daemon_pid"])
            return daemon if daemon and daemon["parent_pid"] == job["pid"] else None
        daemon = wait_for(adopted, timeout=5, description="double-fork descendant adopted by this job's supervisor")
        daemon = {**daemon, "role": "daemon", "job_id": job["job_id"]}
        self.tree_identities.append(daemon)
        identities.append(daemon)
        self.check(still_running(daemon) and daemon["session_pid"] != job["pid"],
                   f"{label} starts an actual double-fork daemon in a detached session")
        return job, identities

    def stopped(self, identities, label):
        wait_for(lambda: not any(still_present(identity) for identity in identities), timeout=10,
                 description=f"{label} process and descendant reaping")
        self.check(True, f"{label} reaps every observed supervisor, command and daemon instance")

    def native_parent_exit(self):
        job, identities = self.tree("native-exit")
        self.call("ssmcp.fs.write_text", {"path": "native-exit.pids.json.release", "text": "release\\n"})
        final = self.finish_job(job)
        self.check(final["exit_code"] == 17 and final["exit_signal"] is None
                   and not final["running"] and final["error"] is None
                   and final["stdout"]["eof"] and final["stderr"]["eof"]
                   and final["stdout"]["error"] is None and final["stderr"]["error"] is None,
                   "native exit 17 is preserved and detached stdout/stderr holders are closed without a drain timeout")
        self.stopped(identities, "native parent exit")
        self.save("native-parent-exit.json", {"started": job, "final": final,
                                             "observed_processes": identities})

    def cancellation(self):
        cancelled, cancel_ids = self.tree("cancel")
        result = self.call("ssmcp.exec.cancel", {"job_id": cancelled["job_id"]})
        self.check(result["cancelled"] and not result["running"], "cancel returns a final cancellation state")
        self.stopped(cancel_ids, "explicit cancellation")
        timed, timeout_ids = self.tree("timeout", timeout_ms=3000)
        final = self.finish_job(timed)
        self.check(final["timed_out"] and not final["running"], "an execution timeout terminates the job and reports timeout")
        self.stopped(timeout_ids, "execution timeout")
        self.save("cancel-timeout.json", {"cancel": result, "cancel_pids": cancel_ids, "timeout": final, "timeout_pids": timeout_ids})

    def expiration(self):
        # The helper binds three real outputs and defers the target's timestamp
        # change until the unchanged timer holds the store lock before its sweep.
        try:
            outcome = verify_output_expiry(self, timeout_seconds=75.0)
        except OutputExpiryFailure as error:
            self.output_expiry_evidence = error.evidence
            try:
                self.output_expiry_record = self.save("automatic-expiration.json", error.evidence)
            except Exception as evidence_error:
                error.evidence.setdefault("evidence_write_errors", []).append(
                    self.scrub(f"{type(evidence_error).__name__}: {evidence_error}"))
            raise
        self.output_expiry_evidence = outcome
        if (not isinstance(outcome, dict)
                or outcome.get("schema") != "ssmcp.public-output-expiry.v1"
                or outcome.get("status") != "passed"
                or outcome.get("clock_mode") != "fixture_timestamp"
                or outcome.get("aging_point") != "timer_sweep_before_expiry"
                or outcome.get("aging_deferred") is not True
                or outcome.get("armed", {}).get("deferred") is not True
                or outcome.get("armed", {}).get("aging_point") != "timer_sweep_before_expiry"
                or outcome.get("public_path_expiry_observed") is not True
                or outcome.get("automatic_cleanup_observed") is not True
                or outcome.get("timer_witness", {}).get("origin") != "timer"
                or outcome.get("timer_witness", {}).get("aging_point") != "timer_sweep_before_expiry"
                or outcome.get("cleanup", {}).get("stopped_acknowledged") is not True
                or outcome.get("cleanup", {}).get("reaped") is not True
                or outcome.get("cleanup", {}).get("forced") is not False
                or outcome.get("cleanup", {}).get("exit_code") != 0
                or outcome.get("cleanup", {}).get("errors") != []):
            raise AssertionError("public output expiry did not return complete timer and cleanup evidence")
        self.output_expiry_observed = True
        self.output_expiry_record = self.save("automatic-expiration.json", outcome)
        # Successful stages return None; keep Unified.run's fail-closed contract.

    def reload_client(self):
        job, identities = self.tree("reload")
        old_id, token, pid = self.client_id, self.secret, self.client.pid
        reply = self.code('return await call(%s, "ssmcp.client.reload", {});' % json.dumps(old_id))
        self.check(reply.get("isError") is not True, "native reload returns no MCP error")
        accepted = reply["structuredContent"]
        self.check(accepted["accepted"] and accepted["client_id"] == old_id,
                   "reload acknowledgement arrives over the account WebSocket before the connection is replaced")
        replacement = self.reader.read(timeout=30)
        self.validate_connection_record(replacement)
        self.check(self.client.pid == pid and self.client.poll() is None and replacement["client_id"] != old_id
                   and all(replacement[field] == self.original[field] for field in ("title", "name", "cwd")),
                   "reload reconnects the same client process with a new connection ID and preserved metadata")
        self.stopped(identities, "reload")
        self.check(self.rpc("ping", modern=True, secret=token)[0] == 200,
                   "the independent account grant remains valid after the device reloads")
        self.client_id = replacement["client_id"]
        listed = self.public_call("list")["clients"]
        self.check(all(item["id"] != old_id for item in listed),
                   "the disconnected client ID cannot survive as a live route")
        stale = self.rejected("ssmcp.exec.status", {"job_id": job["job_id"]})
        self.check(not self.call("ssmcp.exec.list")["jobs"], "reload starts with no inherited background job handles")
        tools = self.tool_catalog(through_code=True)
        self.check({tool["name"]: tool for tool in tools} == self.initial_catalog,
                   "reload rebuilds the full no-Harness catalog through the new registry")
        old_output_rejected = self.output_unavailable(self.retained_artifact)
        sibling_rejected = self.output_unavailable(self.retained_artifact,
                                                  client_id=self.sibling_id, secret=self.sibling_token)
        self.check(True, "replacement and sibling connections reject the actual old memory output ID through read and stat")
        fresh = super().call("ssmcp.fs.read_text", {"path": "typed-output.txt"})
        self.check(fresh.get("spilled") is True and fresh.get("format") == "mcp_tool_result_json"
                   and fresh["artifact"]["complete"] and fresh["artifact"]["id"] != self.retained_artifact["id"],
                   "the replacement connection creates its own complete memory output from the existing fixture")
        native = json.loads(self.artifact_bytes(fresh["artifact"]))
        self.check(native["isError"] is False and native["structuredContent"]["text"] == "typed-é\n" * 5000
                   and native["structuredContent"]["eof"] is True
                   and json.loads(native["content"][0]["text"]) == native["structuredContent"],
                   "fresh output after reload reconstructs the original fixture and complete native MCP result")
        self.save("reload.json", {"acknowledgement": accepted, "old_client_id": old_id,
                  "new_client_id": self.client_id, "process_pid": pid, "stopped_processes": identities,
                  "old_job_rejected": stale, "old_output_rejected": old_output_rejected,
                  "sibling_old_output_rejected": sibling_rejected, "new_output": fresh["artifact"],
                  "old_oauth_status": 200})

    def disconnect(self):
        job, identities = self.tree("disconnect")
        token = self.secret
        self.stop(self.client)
        self.stopped(identities, "connection shutdown")
        self.check(self.rpc("ping", modern=True, secret=token)[0] == 200,
                   "account authorization survives one device disconnect")
        self.check(self.call("terminal.status", client_id=self.sibling_id, secret=self.sibling_token)["running"],
                   "disconnect cleanup leaves the independently authorized sibling client usable")
        self.save("disconnect.json", {"job": job, "stopped_processes": identities, "old_oauth_status": 200})

    def run(self):
        stages = [("account_oauth", self.bootstrap), ("catalog_no_harness", self.catalog),
                  ("concurrent_execution", self.concurrent_commands), ("utf8_transport", self.utf8_transport),
                  ("output_storage", self.outputs),
                  ("file_skill_audit", self.workspace_audit), ("client_isolation", self.client_isolation),
                  ("cancel_timeout", self.cancellation), ("native_parent_exit", self.native_parent_exit),
                  ("automatic_expiration", self.expiration),
                  ("reload", self.reload_client), ("disconnect", self.disconnect)]
        for name, action in stages:
            self.current_section = name
            started = time.monotonic()
            self.sections[name] = {"status": "running"}
            outcome = action()
            if outcome is None:
                outcome = {"status": "passed"}
            elif not isinstance(outcome, dict) or outcome.get("status") != "not_exercised":
                raise AssertionError(f"{name} returned an invalid acceptance outcome")
            self.sections[name] = {**outcome, "duration_seconds": round(time.monotonic() - started, 3)}

    def save_logs(self):
        for path, log in self.logs:
            if not log.closed:
                log.flush()
            self.save("terminal-" + path.name, self.scrub(path.read_text(errors="replace")).encode())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--timeout", type=float, default=240, help="main scenario deadline in seconds; cleanup is separately bounded")
    parser.add_argument("--report", type=Path, default=Path("target/acceptance/unified.json"))
    parser.add_argument("--evidence-dir", type=Path)
    parser.add_argument("--revision", default="unspecified", help="immutable revision used to build these binaries")
    args = parser.parse_args()
    if not sys.platform.startswith("linux") or not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("this real account/process-identity acceptance suite requires Linux and a positive timeout")
    args.report = args.report.resolve()
    evidence = (args.evidence_dir or args.report.with_name(args.report.stem + "-evidence")).resolve()
    started = time.monotonic()
    failure = None
    report = {"schema": "ssmcp.unified-acceptance.v1", "status": "running", "revision": args.revision,
              "transport": "real local account WebSocket + OAuth PKCE + HTTP MCP + independent processes + PTY",
              "model_calls_attempted": 0, "installed_harnesses_exposed": [],
              "timeout_seconds": args.timeout, "coverage_complete": False, "coverage_gaps": [],
              "mechanism_evidence": None, "mechanism_evidence_status": "not_observed",
              "scope": "no-Harness unified runtime; installed native Harness discovery/approval needs its separate acceptance runs"}
    def expired(_signum, _frame):
        raise TimeoutError(f"unified acceptance exceeded its {args.timeout:g}-second deadline")
    previous = signal.signal(signal.SIGALRM, expired)
    with tempfile.TemporaryDirectory(prefix="ssmcp-unified-e2e-") as temporary:
        harness = Unified(Path(temporary), args.bin_dir, evidence)
        # Use the existing router-fixture lifetime knob; preparation and the
        # real 60-second sweep remain inside the original body deadline.
        harness.fixture_timeout_seconds = max(30, min(1800, math.ceil(args.timeout)))
        try:
            signal.setitimer(signal.ITIMER_REAL, args.timeout)
            report["binaries"] = {name: {"path": str((args.bin_dir / name).resolve()),
                                  "sha256": hashlib.sha256((args.bin_dir / name).read_bytes()).hexdigest()}
                                  for name in ("ssmcp", "ssmcpd")}
            harness.run()
            report["status"] = ("passed" if harness.sections
                                and all(section["status"] == "passed" for section in harness.sections.values())
                                else "incomplete")
        except Exception as error:
            failure = error
            report["status"] = "failed"
            report["failure"] = harness.scrub(f"{type(error).__name__}: {error}")
            harness.sections[harness.current_section] = {"status": "failed", "error": report["failure"]}
            try:
                harness.diagnostics()
            except Exception as diagnostic_error:
                report["diagnostic_error"] = harness.scrub(str(diagnostic_error))
        finally:
            signal.setitimer(signal.ITIMER_REAL, 0)
            cleanup_errors = []
            for label, action in (("stop clients and owned account router", harness.cleanup),
                                  ("save redacted terminal evidence", harness.save_logs),
                                  ("remove identified test artifacts", harness.remove_test_artifacts)):
                try:
                    action()
                except Exception as cleanup_error:
                    cleanup_errors.append(harness.scrub(f"{label}: {cleanup_error}"))
            remaining = [item for item in harness.tree_identities if still_present(item)]
            report["remaining_observed_processes"] = remaining
            if remaining:
                cleanup_errors.append("an observed test process remained alive or unreaped after client shutdown")
                # Record the product failure before removing only the exact test
                # process instances. A pidfd avoids signalling a reused PID.
                for identity in remaining:
                    try:
                        fd = os.pidfd_open(identity["pid"])
                        try:
                            info = Path(f"/proc/self/fdinfo/{fd}").read_text()
                            bound_pid = next((int(line.split(":", 1)[1]) for line in info.splitlines()
                                              if line.startswith("Pid:")), None)
                            if bound_pid != identity["pid"]:
                                raise RuntimeError("fixture pidfd could not be bound to its procfs identity")
                            if still_running(identity):
                                signal.pidfd_send_signal(fd, signal.SIGKILL)
                        finally:
                            os.close(fd)
                    except ProcessLookupError:
                        pass
                    except Exception as cleanup_error:
                        cleanup_errors.append(harness.scrub(f"fixture fallback cleanup: {cleanup_error}"))
            if cleanup_errors:
                report["status"] = "failed"
                report["cleanup_errors"] = cleanup_errors
                failure = failure or AssertionError("; ".join(cleanup_errors))
            report.update({"checks": harness.checks, "sections": harness.sections, "evidence": harness.evidence,
                           "account_router_lifetime_seconds": harness.fixture_timeout_seconds,
                           "mechanism_evidence": harness.output_expiry_evidence,
                           "mechanism_evidence_file": harness.output_expiry_record,
                           "mechanism_evidence_status": (
                               "observed_local_public_path" if harness.output_expiry_observed
                               else "failed" if harness.output_expiry_evidence is not None else "not_observed"),
                           "coverage_complete": report["status"] == "passed",
                           "coverage_gaps": [{"section": name, "status": section["status"],
                                              "reason": section["reason"]}
                                             for name, section in harness.sections.items()
                                             if section.get("status") == "not_exercised"],
                           "duration_seconds": round(time.monotonic() - started, 3)})
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
            args.report.chmod(0o600)
            signal.signal(signal.SIGALRM, previous)
    print(f"\nUnified acceptance {report['status']}: {len(report['checks'])} checks; report {args.report}", flush=True)
    if failure is not None or report["status"] != "passed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
