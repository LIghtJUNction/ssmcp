"""Observe public RAM output expiration through the real periodic janitor.

The caller owns the prepared AccountGateway, OAuth grant and overall alarm.
This helper never runs Cargo, starts a model, extends that alarm, or exposes a
production clock override.  Only the private ignored Rust fixture may age the
completed target.  Its timer witness must arrive before another output access.
"""

import hashlib
import json
import math
import os
from pathlib import Path
import re
import selectors
import signal
import socket
import stat
import subprocess
import sys
import threading
import time


FIXTURE_TEST = "client::output_expiry_fixture::public_output_expiry_client_fixture"
FIXTURE_FD = "SSMCP_OUTPUT_EXPIRY_FIXTURE_FD"
FIXTURE_CONFIG = "SSMCP_OUTPUT_EXPIRY_FIXTURE_CONFIG"
FRAME_LIMIT = 65536
SCENARIO_SECONDS = 75.0
CLEANUP_SECONDS = 20.0
RETENTION_SECONDS = 900
SWEEP_SECONDS = 60
PAYLOADS = {"target": b"T\n" * 600, "fresh": b"F\n" * 600, "active": b"A\n" * 600}
OUTPUT_PROGRAM = """\
import json, os, pathlib, sys, time
fd = os.open(sys.argv[3], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, "w") as marker:
    json.dump({"pid": os.getpid()}, marker)
sys.stdout.buffer.write(bytes.fromhex(sys.argv[1]))
sys.stdout.buffer.flush()
deadline = time.monotonic() + 90
while not pathlib.Path(sys.argv[2]).exists():
    if time.monotonic() >= deadline:
        raise SystemExit(91)
    time.sleep(0.02)
"""


class OutputExpiryFailure(AssertionError):
    """Preserve the first failure and the independent cleanup observations."""

    def __init__(self, message, evidence):
        super().__init__(message)
        self.evidence = evidence


class Deadline:
    def __init__(self, seconds, *, clock=None):
        self.clock = clock or time.monotonic
        self.started = self.clock()
        self.ends = self.started + seconds

    def remaining(self, maximum=None):
        left = self.ends - self.clock()
        if left <= 0:
            raise TimeoutError("public output expiry scenario exceeded its absolute deadline")
        return min(left, maximum) if maximum is not None else left

    def elapsed(self):
        return self.clock() - self.started


class ScenarioAlarm:
    """Interrupt blocking HTTP at the earlier of two unchanged absolute ends."""

    def __init__(self, deadline):
        self.deadline = deadline
        self.installed = False
        self.timeout_origin = None
        self.outer_fired = False
        self.outer_elapsed = False

    def arm(self):
        _check(threading.current_thread() is threading.main_thread(),
               "output expiry requires the Unified main thread")
        anchor = self.deadline.clock()
        remaining, self.interval = signal.getitimer(signal.ITIMER_REAL)
        self.handler = signal.getsignal(signal.SIGALRM)
        _check(remaining > 0 and callable(self.handler),
               "output expiry requires the existing Unified overall alarm")
        self.outer_end = anchor + remaining
        self.outer_first = self.outer_end <= self.deadline.ends
        duration = min(self.outer_end, self.deadline.ends) - self.deadline.clock()
        if duration <= 0:
            raise TimeoutError("output expiry started after its effective absolute deadline")
        signal.signal(signal.SIGALRM, self._expired)
        self.installed = True
        signal.setitimer(signal.ITIMER_REAL, duration)

    def _expired(self, signum, frame):
        if self.outer_first:
            self.outer_fired = True
            self.timeout_origin = "overall"
            self.handler(signum, frame)
            raise TimeoutError("the existing overall alarm returned without stopping the scenario")
        self.timeout_origin = "scenario"
        raise TimeoutError("public output expiry scenario exceeded its absolute deadline")

    def restore(self, *, failure_pending):
        if not self.installed:
            return
        signal.setitimer(signal.ITIMER_REAL, 0)
        signal.signal(signal.SIGALRM, self.handler)
        self.installed = False
        remaining = self.outer_end - self.deadline.clock()
        self.outer_elapsed = remaining <= 0
        # Restore the original interval too. An already-fired one-shot alarm is
        # left disarmed, never rearmed at epsilon while cleanup is in progress.
        signal.setitimer(signal.ITIMER_REAL,
                         0 if self.outer_fired or self.outer_elapsed else remaining,
                         self.interval)
        if self.outer_elapsed and not self.outer_fired and not failure_pending:
            self.outer_fired = True
            self.timeout_origin = "overall"
            self.handler(signal.SIGALRM, None)
            raise TimeoutError("the existing overall alarm returned after its deadline")

    def observation(self):
        return {"timeout_origin": self.timeout_origin, "overall_alarm_fired": self.outer_fired,
                "overall_deadline_elapsed": self.outer_elapsed,
                "original_handler_restored": not self.installed}


def _reject_constant(_value):
    raise ValueError("non-finite JSON number")


class Control:
    """Bounded newline JSON on the single inherited private socket."""

    def __init__(self, stream):
        self.stream = stream
        self.stream.setblocking(False)
        self.pending = bytearray()

    def _wait(self, event, deadline):
        with selectors.DefaultSelector() as selector:
            selector.register(self.stream, event)
            if not selector.select(deadline.remaining()):
                deadline.remaining()
                raise TimeoutError("private output expiry control socket timed out")

    def send(self, value, deadline):
        data = json.dumps(value, separators=(",", ":"), allow_nan=False).encode() + b"\n"
        if len(data) > FRAME_LIMIT:
            raise AssertionError("private output expiry request exceeded its frame limit")
        view = memoryview(data)
        while view:
            deadline.remaining()
            try:
                written = self.stream.send(view)
            except BlockingIOError:
                self._wait(selectors.EVENT_WRITE, deadline)
                continue
            if not written:
                raise EOFError("private output expiry control socket closed during send")
            view = view[written:]

    def receive(self, deadline):
        while True:
            deadline.remaining()
            newline = self.pending.find(b"\n")
            if newline >= 0:
                if newline + 1 > FRAME_LIMIT:
                    raise AssertionError("private output expiry response exceeded its frame limit")
                line = bytes(self.pending[:newline])
                del self.pending[:newline + 1]
                try:
                    value = json.loads(line.decode("utf-8"), parse_constant=_reject_constant)
                except (UnicodeDecodeError, ValueError) as error:
                    raise AssertionError("invalid private output expiry JSON frame") from error
                if (not isinstance(value, dict) or type(value.get("schema_version")) is not int
                        or value["schema_version"] != 1):
                    raise AssertionError("private output expiry response has an invalid schema")
                return value
            if len(self.pending) >= FRAME_LIMIT:
                raise AssertionError("private output expiry response exceeded its frame limit")
            self._wait(selectors.EVENT_READ, deadline)
            try:
                chunk = self.stream.recv(min(4096, FRAME_LIMIT + 1 - len(self.pending)))
            except BlockingIOError:
                continue
            if not chunk:
                raise EOFError("private output expiry fixture closed before the required response")
            self.pending.extend(chunk)

    def close(self):
        self.stream.close()


def _check(condition, message):
    if not condition:
        raise AssertionError(message)


def _binding(ready, process_id):
    value = ready.get("binding")
    _check(ready.get("event") == "ready" and ready.get("runtime_bound") is True
           and ready.get("retention_secs") == RETENTION_SECONDS
           and ready.get("sweep_secs") == SWEEP_SECONDS,
           "fixture ready did not bind the real runtime and unchanged output policy")
    _check(isinstance(value, dict)
           and set(value) == {"process_id", "client_id", "connection_attempt", "store_id"}
           and type(value["process_id"]) is int and value["process_id"] == process_id
           and type(value["connection_attempt"]) is int and value["connection_attempt"] == 1
           and isinstance(value["client_id"], str) and bool(value["client_id"])
           and isinstance(value["store_id"], str)
           and re.fullmatch(r"[0-9a-f]+", value["store_id"]) is not None,
           "fixture ready has an invalid process, connection or store binding")
    return value


def _event(value, expected, binding):
    if value.get("event") == "error":
        # The complete frame is private evidence, never interpreted as success.
        raise AssertionError("Rust output expiry fixture reported an error: "
                             + json.dumps(value, ensure_ascii=False, allow_nan=False))
    _check(value.get("event") == expected and value.get("binding") == binding,
           f"expected {expected} for the exact original output fixture binding")
    return value


def _descriptor(metadata):
    return {field: metadata[field] for field in ("id", "bytes", "sha256")}


def _armed(value, binding, expected, target_metadata):
    _event(value, "armed", binding)
    _check(all(value.get(name) == metadata for name, metadata in expected.items()),
           "age acknowledgement changed a selected output descriptor")
    old, new = value.get("old_expires_at_unix"), value.get("new_expires_at_unix")
    _check(type(old) is int and old == target_metadata["expires_at_unix"]
           and type(new) is int and 0 <= new < old and new <= time.time(),
           "age acknowledgement did not expire only the completed target timestamp")
    return value


def _timer_removed(value, binding, expected):
    _event(value, "timer_removed", binding)
    _check(value.get("origin") == "timer" and value.get("removed") == expected["target"],
           "output removal was not an exact timer-origin target witness")
    controls = value.get("controls")
    _check(isinstance(controls, dict)
           and controls.get("fresh") == {
               "id": expected["fresh"]["id"], "present": True, "active": False}
           and controls.get("active") == {
               "id": expected["active"]["id"], "present": True, "active": True}
           and controls["fresh"]["present"] is True and controls["fresh"]["active"] is False
           and controls["active"]["present"] is True and controls["active"]["active"] is True,
           "timer witness did not preserve the exact fresh and active controls")
    return value


def _pin(executable):
    path = Path(executable).resolve(strict=True)
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW)
    try:
        before = os.fstat(descriptor)
        _check(stat.S_ISREG(before.st_mode) and os.access(path, os.X_OK),
               "output expiry input must be a regular executable")
        digest = hashlib.sha256()
        while chunk := os.read(descriptor, 1024 * 1024):
            digest.update(chunk)
        after = os.fstat(descriptor)
        current = path.stat()
        identity = lambda item: (item.st_dev, item.st_ino, item.st_size, item.st_mtime_ns)
        _check(identity(before) == identity(after) == identity(current),
               "output expiry executable changed while its identity was read")
        return {"path": str(path), "bytes": before.st_size, "sha256": digest.hexdigest()}
    finally:
        os.close(descriptor)


def _request(harness, deadline, action):
    previous = getattr(harness, "http_timeout", None)
    had_previous = hasattr(harness, "http_timeout")
    harness.http_timeout = deadline.remaining(min(float(previous or 10), 10))
    try:
        value = action()
        deadline.remaining()
        return value
    finally:
        if had_previous:
            harness.http_timeout = previous
        else:
            del harness.http_timeout


def _native(harness, deadline, client_id, name, arguments):
    return _request(harness, deadline, lambda: harness.call(
        name, arguments, client_id=client_id))


def _job_output(harness, deadline, client_id, job, *, complete):
    while True:
        result = _native(harness, deadline, client_id, "ssmcp.exec.wait",
                         {"job_id": job["job_id"], "wait_ms": 50})
        if complete and result.get("running") is False:
            _check(result.get("exit_code") == 0 and result.get("error") is None
                   and result.get("stdout", {}).get("eof") is True
                   and result.get("stderr", {}).get("eof") is True,
                   "output fixture command did not complete successfully with both EOFs")
            return result
        if not complete:
            _check(result.get("running") is True, "held output fixture exited prematurely")
            metadata = result.get("stdout", {}).get("artifact")
            if metadata and metadata.get("bytes") == len(PAYLOADS["active"]):
                return result
        time.sleep(min(0.02, deadline.remaining()))


def _read_output(harness, deadline, client_id, metadata, expected, *, complete):
    actual = _native(harness, deadline, client_id, "ssmcp.output.stat", {"id": metadata["id"]})
    harness.artifact_metadata(actual, client_id=client_id)
    _check(actual == metadata and actual["complete"] is complete
           and actual["bytes"] == len(expected) and actual["observed_bytes"] == len(expected)
           and actual["sha256"] == hashlib.sha256(expected).hexdigest(),
           "public output stat changed the expected content, completion or identity")
    observed = _request(harness, deadline, lambda: harness.artifact_bytes(
        actual, client_id=client_id))
    _check(observed == expected, "public output pages did not preserve the actual fixture bytes")
    return {"metadata": actual, "read_bytes": len(observed),
            "read_sha256": hashlib.sha256(observed).hexdigest()}


def _scrub(harness, text):
    if hasattr(harness, "scrub"):
        return harness.scrub(text)
    for secret in [getattr(harness, "secret", ""), *getattr(harness, "sensitive", [])]:
        if secret:
            text = text.replace(secret, "[redacted]")
    return text


def _process_identity(pid):
    """Same Linux instance identity used by Unified's owned-process checks."""
    try:
        fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return {"pid": pid, "start_ticks": int(fields[19]), "state": fields[0],
                "parent_pid": int(fields[1]), "session_pid": int(fields[3])}
    except FileNotFoundError:
        return None


def _same_instance(identity):
    current = _process_identity(identity["pid"])
    return current if current and current["start_ticks"] == identity["start_ticks"] else None


def _exact_pidfd(identity):
    if _same_instance(identity) is None:
        return None
    try:
        descriptor = os.pidfd_open(identity["pid"])
    except ProcessLookupError:
        return None
    try:
        info = Path(f"/proc/self/fdinfo/{descriptor}").read_text()
        bound_pid = next((int(line.split(":", 1)[1]) for line in info.splitlines()
                          if line.startswith("Pid:")), None)
        if bound_pid != identity["pid"] or _same_instance(identity) is None:
            os.close(descriptor)
            return None
        return descriptor
    except BaseException:
        os.close(descriptor)
        raise


def _signal_instance(identity, signum):
    descriptor = _exact_pidfd(identity)
    if descriptor is None:
        return False
    try:
        signal.pidfd_send_signal(descriptor, signum)
        return True
    except ProcessLookupError:
        return False
    finally:
        os.close(descriptor)


class OwnedProcesses:
    """Observe only this fixture's descendants; never search by UID or name."""

    def __init__(self, process, registry=None):
        self.process = process
        self.registry = registry
        self.instances = {}
        root = _process_identity(process.pid)
        _check(root is not None, "fixture exited before its process instance could be bound")
        self.root = self._remember(root, "output_expiry_fixture")

    def _remember(self, identity, role):
        key = (identity["pid"], identity["start_ticks"])
        if key not in self.instances:
            record = {**identity, "role": role}
            self.instances[key] = record
            if self.registry is not None:
                self.registry.append(record)
        return self.instances[key]

    def refresh(self, deadline=None):
        # Each children file belongs to a thread of an already-bound process.
        # Session/group changes do not break the parent relationship.
        pending = list(self.instances.values())
        visited = set()
        while pending:
            if deadline is not None:
                deadline.remaining()
            parent = pending.pop()
            key = (parent["pid"], parent["start_ticks"])
            if key in visited:
                continue
            visited.add(key)
            if _same_instance(parent) is None:
                continue
            try:
                paths = list(Path(f"/proc/{parent['pid']}/task").glob("*/children"))
            except FileNotFoundError:
                continue
            for path in paths:
                if deadline is not None:
                    deadline.remaining()
                try:
                    children = path.read_text()
                except FileNotFoundError:
                    continue
                _check(len(children) <= 65536, "owned fixture children observation exceeded its bound")
                for value in children.split():
                    pid = int(value)
                    child = _process_identity(pid)
                    if (child is None or child["parent_pid"] != parent["pid"]
                            or child["start_ticks"] < parent["start_ticks"]
                            or _same_instance(parent) is None):
                        continue
                    record = self._remember(child, "output_expiry_descendant")
                    _check(len(self.instances) <= 256,
                           "owned fixture process evidence exceeded its bound")
                    pending.append(record)
        return _same_instance(self.root) is not None

    def bind_job(self, job, command_pid, deadline):
        self.refresh(deadline)
        _check(type(job.get("pid")) is int and type(command_pid) is int,
               "public job and command PID must be actual integer process IDs")
        supervisor, command = _process_identity(job["pid"]), _process_identity(command_pid)
        _check(job.get("pid_role") == "supervisor" and supervisor is not None and command is not None
               and supervisor["state"] != "Z" and command["state"] != "Z"
               and command_pid != job["pid"] and command["parent_pid"] == job["pid"],
               "the reported supervisor does not own the separately observed live command")
        for role, identity in (("supervisor", supervisor), ("command", command)):
            key = (identity["pid"], identity["start_ticks"])
            _check(key in self.instances,
                   "public job PID is not a verified descendant of this fixture instance")
            self.instances[key].update(role="output_expiry_" + role, job_id=job["job_id"])
        return {"supervisor": dict(self.instances[(supervisor["pid"], supervisor["start_ticks"])]),
                "command": dict(self.instances[(command["pid"], command["start_ticks"])])}

    def snapshot(self):
        return [dict(self.instances[key]) for key in sorted(self.instances)]

    def remaining(self):
        self.process.poll()  # Popen alone reaps its direct fixture child.
        remaining = []
        for identity in self.instances.values():
            if _same_instance(identity) is None:
                continue
            if identity["pid"] != self.root["pid"] and hasattr(os, "P_PIDFD"):
                descriptor = _exact_pidfd(identity)
                if descriptor is not None:
                    try:
                        try:
                            os.waitid(os.P_PIDFD, descriptor, os.WEXITED | os.WNOHANG)
                        except ChildProcessError:
                            pass  # Its actual parent is responsible for reaping.
                    finally:
                        os.close(descriptor)
            current = _same_instance(identity)
            if current is not None:
                remaining.append({**identity, "state_now": current["state"]})
        return remaining

    def signal(self, identity, signum):
        if identity.get("state_now") == "Z":
            return False
        return _signal_instance(identity, signum)


def _wait_owned_gone(owned, deadline, seconds):
    end = min(deadline.ends, deadline.clock() + seconds)
    remaining = owned.remaining()
    while remaining and deadline.clock() < end:
        time.sleep(min(0.02, max(0, end - deadline.clock())))
        remaining = owned.remaining()
    return remaining


def _cleanup(process, control, binding, releases, owned=None):
    deadline = Deadline(CLEANUP_SECONDS)
    result = {"budget_seconds": CLEANUP_SECONDS, "stopped_acknowledged": False,
              "forced": False, "exit_code": None, "reaped": process is None, "errors": [],
              "scope_observed_before_stop": False, "observed_instances": [],
              "remaining_instances": [], "all_observed_instances_gone": process is None}
    release_paths = list(releases.values()) if isinstance(releases, dict) else (
        [] if releases is None else [releases])
    for release in release_paths:
        try:
            if not release.exists():
                release.write_bytes(b"release\n")
        except Exception as error:
            result["errors"].append(f"release held fixture: {type(error).__name__}: {error}")
    if process is not None:
        try:
            if owned is None:
                owned = OwnedProcesses(process)
            result["scope_observed_before_stop"] = owned.refresh(deadline)
            if not result["scope_observed_before_stop"]:
                result["errors"].append("fixture exited before its final owned-process observation")
        except Exception as error:
            result["errors"].append(f"observe owned fixture processes: {type(error).__name__}: {error}")
    if control is not None:
        try:
            if process is not None and process.poll() is None and binding is not None:
                stop_deadline = Deadline(min(12.0, deadline.remaining()))
                control.send({"schema_version": 1, "op": "stop", "binding": binding}, stop_deadline)
                for _ in range(4):
                    value = control.receive(stop_deadline)
                    if value.get("event") == "stopped":
                        _event(value, "stopped", binding)
                        _check(value.get("cleanup_completed") is True,
                               "fixture did not confirm the normal runtime cleanup returned")
                        result["stopped_acknowledged"] = True
                        break
                    _check(value.get("binding") == binding,
                           "cleanup received another fixture binding")
                    result["errors"].append(
                        "unexpected private response during cleanup: "
                        + json.dumps(value, ensure_ascii=False, allow_nan=False))
                _check(result["stopped_acknowledged"], "fixture did not acknowledge completed cleanup")
        except Exception as error:
            result["errors"].append(f"stop fixture: {type(error).__name__}: {error}")
        finally:
            try:
                control.close()
            except Exception as error:
                result["errors"].append(f"close fixture control: {type(error).__name__}: {error}")
    if process is not None:
        try:
            process.wait(timeout=min(2.0 if binding is not None else 12.0, deadline.remaining()))
        except subprocess.TimeoutExpired:
            pass  # Exact-instance fallback below also covers separate sessions.
        except Exception as error:
            result["errors"].append(f"reap fixture: {type(error).__name__}: {error}")
        if owned is not None:
            remaining = None
            try:
                remaining = _wait_owned_gone(owned, deadline, min(2.0, deadline.remaining()))
            except Exception as error:
                result["errors"].append(f"observe runtime cleanup: {type(error).__name__}: {error}")
            if remaining is None:
                try:
                    remaining = owned.remaining()
                except Exception as error:
                    result["errors"].append(f"read remaining instances: {type(error).__name__}: {error}")
                    remaining = owned.snapshot()
            if remaining:
                result["forced"] = True
                result["errors"].append("owned process instances remained after normal runtime cleanup")
                for signum, maximum in ((signal.SIGTERM, 2.0), (signal.SIGKILL, None)):
                    for identity in remaining:
                        try:
                            owned.signal(identity, signum)
                        except Exception as error:
                            result["errors"].append(
                                f"signal owned instance {identity['pid']}: {type(error).__name__}: {error}")
                    try:
                        left = deadline.remaining()
                        remaining = _wait_owned_gone(
                            owned, deadline, left if maximum is None else min(maximum, left))
                    except Exception as error:
                        result["errors"].append(f"wait owned cleanup: {type(error).__name__}: {error}")
                        try:
                            remaining = owned.remaining()
                        except Exception as observation_error:
                            result["errors"].append(
                                f"observe forced cleanup: {type(observation_error).__name__}: {observation_error}")
                            # Keep the last known bound instances for the KILL
                            # attempt; an observation error is not absence.
                    if not remaining:
                        break
            try:
                result["observed_instances"] = owned.snapshot()
                result["remaining_instances"] = owned.remaining()
                result["all_observed_instances_gone"] = not result["remaining_instances"]
            except Exception as error:
                result["errors"].append(f"final owned-process proof: {type(error).__name__}: {error}")
                result["all_observed_instances_gone"] = False
        else:
            result["all_observed_instances_gone"] = False
            result["errors"].append("owned process identity evidence is unavailable")
        result["exit_code"] = process.poll()
        result["reaped"] = result["exit_code"] is not None
        if result["exit_code"] != 0:
            result["errors"].append("output expiry library fixture did not exit successfully")
        if binding is not None and not result["stopped_acknowledged"]:
            result["errors"].append("normal runtime cleanup was not confirmed")
        if not result["all_observed_instances_gone"]:
            result["errors"].append("an observed owned process is still present or could not be proved reaped")
    finished_at = deadline.clock()
    result["duration_seconds"] = round(finished_at - deadline.started, 3)
    if finished_at >= deadline.ends:
        result["errors"].append(
            "owned output expiry cleanup exceeded its 20-second absolute deadline")
    return result


def _finish(evidence, failure):
    cleanup_errors = evidence["cleanup"]["errors"]
    if failure is not None or cleanup_errors:
        evidence["status"] = "failed"
        if failure is not None:
            evidence["failure"] = failure
        message = failure or "output expiry cleanup failed"
        raise OutputExpiryFailure(message, evidence)
    evidence["status"] = "passed"
    return evidence


def run(harness, timeout_seconds=SCENARIO_SECONDS):
    """Return actual evidence, or raise OutputExpiryFailure with .evidence.

    The caller saves both outcomes. No passed evidence is returned until the
    original private fixture has acknowledged cleanup and exited successfully.
    """
    if (type(timeout_seconds) not in (int, float) or not math.isfinite(timeout_seconds)
            or not 0 < timeout_seconds <= SCENARIO_SECONDS):
        raise ValueError("output expiry scenario timeout must be in (0, 75] seconds")
    deadline = Deadline(timeout_seconds)
    evidence = {"schema": "ssmcp.public-output-expiry.v1", "status": "running",
                "clock_mode": "fixture_timestamp", "retention_secs": RETENTION_SECONDS,
                "sweep_interval_secs": SWEEP_SECONDS, "scenario_timeout_seconds": timeout_seconds,
                "transport": "owned loopback HTTP/WS account, OAuth, public code_mode and native output",
                "natural_900_seconds_observed": False, "tls_handshake_exercised": False,
                "public_path_expiry_observed": False, "automatic_cleanup_observed": False}
    process = control = child_socket = binding = release = alarm = owned = None
    releases = None
    primary = None
    label = "output-expiry"
    try:
        alarm = ScenarioAlarm(deadline)
        alarm.arm()
        _check(sys.platform.startswith("linux") and harness.account_gateway.prepared,
               "output expiry requires the already prepared owned Linux account gateway")
        fixture = getattr(harness, "fixture_binary", None)
        _check(fixture is not None, "the exact already-built library fixture is required")
        client_pin, fixture_pin = _pin(harness.binary_dir / "ssmcp"), _pin(fixture)
        evidence["binaries"] = {"ssmcp": client_pin, "library_fixture": fixture_pin}
        deadline.remaining()
        directory = harness.directory / "output-expiry-workspace"
        directory.mkdir(mode=0o700)
        releases = {name: directory / f"{name}.release" for name in PAYLOADS}
        release = releases["active"]
        command = harness.client_command(name="unified-output-expiry")
        _check(str(Path(command[0]).resolve(strict=True)) == client_pin["path"],
               "gateway command did not select the bound real client executable")
        environment = harness.client_environment(label)
        parent_socket, child_socket = socket.socketpair(socket.AF_UNIX, socket.SOCK_STREAM)
        control = Control(parent_socket)
        environment[FIXTURE_FD] = str(child_socket.fileno())
        environment[FIXTURE_CONFIG] = json.dumps({
            "schema_version": 1, "executable": client_pin["path"],
            "executable_sha256": client_pin["sha256"], "argv": command[1:]})
        log_path = harness.directory / "output-expiry-fixture.log"
        log_fd = os.open(log_path, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        log = os.fdopen(log_fd, "wb")
        harness.logs.append((log_path, log))
        deadline.remaining()
        process = subprocess.Popen(
            [fixture_pin["path"], "--exact", FIXTURE_TEST, "--ignored", "--nocapture"],
            stdin=subprocess.DEVNULL, stdout=log, stderr=log, cwd=directory,
            env=environment, start_new_session=True, pass_fds=(child_socket.fileno(),))
        harness.processes.append(process)  # Ownership precedes every ready wait.
        owned = OwnedProcesses(process, registry=getattr(harness, "tree_identities", None))
        owned.refresh(deadline)
        child_socket.close()
        child_socket = None
        ready = control.receive(deadline)
        if ready.get("event") == "error":
            _event(ready, "ready", None)
        binding = _binding(ready, process.pid)
        evidence["ready"] = ready
        client_id = binding["client_id"]
        harness.validate_connection_record({"client_id": client_id})
        listed = _request(harness, deadline, lambda: harness.public_call("list"))
        _check(process.poll() is None
               and any(row.get("id") == client_id for row in listed["clients"]),
               "the fixture runtime is not the same live account-authorized public client")
        harness.check(True, "output expiry binds one real client, runtime and unchanged RAM policy")

        initial, jobs, metadata, job_processes = {}, {}, {}, {}
        evidence.update(jobs=jobs, public_before=initial, job_processes=job_processes)
        for name, payload in PAYLOADS.items():
            pid_path = directory / f"{name}.pid.json"
            args = [str(Path(sys.executable).resolve()), "-u", "-c", OUTPUT_PROGRAM,
                    payload.hex(), str(releases[name]), str(pid_path)]
            job = _native(harness, deadline, client_id, "ssmcp.exec.start",
                          {"command": args, "timeout_ms": 90000})
            _check(isinstance(job.get("job_id"), str) and bool(job["job_id"]),
                   "public exec did not return a real fixture job")
            jobs[name] = job["job_id"]
            held = _job_output(harness, deadline, client_id, job, complete=False)
            with pid_path.open("rb") as marker:
                encoded = marker.read(1025)
            _check(len(encoded) <= 1024, "owned command PID record exceeded its bound")
            observed = json.loads(encoded)
            _check(isinstance(observed, dict) and set(observed) == {"pid"}
                   and type(observed["pid"]) is int and observed["pid"] > 0,
                   "owned command did not produce a valid private PID record")
            job_processes[name] = owned.bind_job(job, observed["pid"], deadline)
            harness.check(True, "output expiry binds the live supervisor and command to its own fixture tree")
            if name != "active":
                releases[name].write_bytes(b"release\n")
                final = _job_output(harness, deadline, client_id, job, complete=True)
            else:
                final = held
            metadata[name] = final["stdout"]["artifact"]
            initial[name] = _read_output(harness, deadline, client_id, metadata[name],
                                         payload, complete=name != "active")
        _check(len({value["id"] for value in metadata.values()}) == 3,
               "target and output controls did not have three distinct real IDs")
        expected = {name: _descriptor(value) for name, value in metadata.items()}
        harness.check(True, "public exec and output reads verify completed target, fresh and real active captures")

        age_started = time.monotonic()
        control.send({"schema_version": 1, "op": "age", "binding": binding, **expected}, deadline)
        armed = _armed(control.receive(deadline), binding, expected, metadata["target"])
        evidence["armed"] = armed
        # No public call, output allocation, read/stat or manual cleanup belongs
        # between age and this passive, exact timer-origin witness.
        witness = _timer_removed(control.receive(deadline), binding, expected)
        evidence["timer_witness"] = witness
        evidence["observed_after_age_seconds"] = round(time.monotonic() - age_started, 3)
        evidence["automatic_cleanup_observed"] = True
        _check(process.poll() is None, "fixture disconnected before public expiry verification")
        unavailable = _request(harness, deadline, lambda: harness.output_unavailable(
            metadata["target"], client_id=client_id))
        evidence["public_unavailable"] = unavailable
        evidence["public_path_expiry_observed"] = True
        after = {name: _read_output(harness, deadline, client_id, metadata[name],
                                    PAYLOADS[name], complete=name == "fresh")
                 for name in ("fresh", "active")}
        evidence["public_controls_after"] = after
        harness.check(True, "real timer removal precedes public unavailability while fresh and active bytes remain")

        release.write_bytes(b"release\n")
        finished = _job_output(harness, deadline, client_id, {"job_id": jobs["active"]}, complete=True)
        evidence["active_completed"] = _read_output(
            harness, deadline, client_id, finished["stdout"]["artifact"],
            PAYLOADS["active"], complete=True)
        harness.check(True, "the unchanged active capture completes normally after its explicit release")
        deadline.remaining()
    except Exception as error:
        primary = error
    finally:
        evidence["scenario_duration_seconds"] = round(deadline.elapsed(), 3)
        if alarm is not None:
            try:
                alarm.restore(failure_pending=primary is not None)
            except Exception as error:
                if primary is None:
                    primary = error
                else:
                    evidence["alarm_restore_error"] = _scrub(
                        harness, f"{type(error).__name__}: {error}")
            evidence["deadline"] = alarm.observation()
        child_close_error = None
        if child_socket is not None:
            try:
                child_socket.close()
            except Exception as error:
                child_close_error = f"close inherited fixture socket: {type(error).__name__}: {error}"
        try:
            cleanup = _cleanup(process, control, binding, releases, owned)
        except Exception as error:
            cleanup = {"errors": [f"cleanup exception: {type(error).__name__}: {error}"],
                       "reaped": False, "exit_code": process.poll() if process is not None else None}
        if child_close_error is not None:
            cleanup["errors"].append(child_close_error)
        cleanup["errors"] = [_scrub(harness, value) for value in cleanup["errors"]]
        evidence["cleanup"] = cleanup
    if hasattr(harness, "external_invocations"):
        try:
            invocations = harness.external_invocations(label) if process is not None else []
            evidence["optional_provider_invocations"] = invocations
            _check(not invocations, "output expiry fixture invoked an optional external provider")
        except Exception as error:
            if primary is None:
                primary = error
            else:
                evidence["postcheck_error"] = _scrub(harness, f"{type(error).__name__}: {error}")
    failure = _scrub(harness, f"{type(primary).__name__}: {primary}") if primary is not None else None
    try:
        return _finish(evidence, failure)
    except OutputExpiryFailure as error:
        raise error from primary
