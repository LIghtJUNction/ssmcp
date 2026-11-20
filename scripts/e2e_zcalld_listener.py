#!/usr/bin/env python3
"""Real composer /send -> native zcalld listen/serve -> fake HTTP provider -> original history.

All services, credentials, terminals and model responses are owned fixtures.
Requires prebuilt immutable executables; this script never compiles or calls a real model.
"""
import argparse
import hashlib
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import queue
import re
import signal
import socket
import subprocess
import tempfile
import threading
import time
import traceback

from e2e_force_send import ForceSend, ENTER, text
from e2e_listener_transports import Program


def digest(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


class Plan:
    def __init__(self, content, status=200):
        self.content, self.status = content, status
        self.started, self.release = threading.Event(), threading.Event()
        self.request = None


class Provider:
    def __init__(self):
        self.plans, self.all_plans, self.requests = queue.Queue(), [], []
        owner = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *_args):
                pass

            def do_POST(self):
                length = int(self.headers.get("Content-Length", "0"))
                if self.path != "/v1/chat/completions" or not 0 < length <= 1024 * 1024:
                    self.send_error(400)
                    return
                body = json.loads(self.rfile.read(length))
                request = {
                    "path": self.path, "body": body,
                    "caller_credential_forwarded": bool(self.headers.get("Authorization")),
                    "unexpected": True,
                }
                # Count every actual HTTP attempt, including an unplanned replay.
                # Waiting for a matching plan must never hide an extra request.
                owner.requests.append(request)
                try:
                    plan = owner.plans.get(timeout=2)
                except queue.Empty:
                    self.send_error(500, "unexpected fixture generation")
                    return
                request["unexpected"] = False
                plan.request = request
                plan.started.set()
                if not plan.release.wait(25):
                    self.send_error(504, "fixture release timeout")
                    return
                if plan.status == 200:
                    result = {
                        "id": "synthetic-completion", "object": "chat.completion",
                        "model": "fixture-upstream-model", "created": 0,
                        "choices": [{"index": 0, "message": {"role": "assistant", "content": plan.content},
                                     "finish_reason": "stop"}],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
                    }
                else:
                    result = {"error": {"message": plan.content}}
                encoded = json.dumps(result, ensure_ascii=False).encode()
                try:
                    self.send_response(plan.status)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(encoded)))
                    self.end_headers()
                    self.wfile.write(encoded)
                except (BrokenPipeError, ConnectionResetError):
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=0.05), daemon=True)
        self.thread.start()

    def plan(self, content, status=200):
        plan = Plan(content, status)
        self.all_plans.append(plan)
        self.plans.put(plan)
        return plan

    def close(self):
        for plan in self.all_plans:
            plan.release.set()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


class Native:
    def __init__(self, harness, endpoint, config, label):
        self.events, self.incoming = [], queue.Queue()
        self.log = (harness.output / (label + ".stderr.log")).open("wb")
        self.process = subprocess.Popen(
            [str(harness.zcalld), "listen", "--config", str(config),
             "--ssmcp-socket", str(endpoint), "--model", "p1-route",
             "--once", "--json", "--timeout-ms", "20000"],
            cwd=harness.workspace, env=harness.native_environment,
            stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=self.log,
            text=True, start_new_session=True,
        )
        harness.processes.append(self.process)

        def read():
            for line in self.process.stdout:
                value = json.loads(line)
                self.events.append(value)
                self.incoming.put(value)
            self.incoming.put(None)

        self.thread = threading.Thread(target=read, daemon=True)
        self.thread.start()

    def event(self, name, front, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            front.pump(0.02)
            try:
                value = self.incoming.get(timeout=0.03)
            except queue.Empty:
                continue
            if value is None:
                raise AssertionError(f"native listener exited before {name}: {self.events}")
            if value.get("event") == name:
                return value["data"]
            if value.get("ok") is False:
                raise AssertionError(f"native listener rejected {name}: {value.get('error')}")
        raise AssertionError(f"native listener did not report {name}")

    def exited(self, expected_success, timeout=10):
        code = self.process.wait(timeout=timeout)
        self.thread.join(timeout=2)
        self.log.close()
        if (code == 0) != expected_success:
            raise AssertionError(f"native exit {code}, expected success={expected_success}: {self.events}")
        return code


class Integration(ForceSend):
    def __init__(self, directory, binary_dir, decoder, output, zcalld):
        super().__init__(directory, binary_dir, decoder, output)
        self.zcalld = zcalld
        self.provider = None
        self.evidence, self.frames = [], []

    def capture(self, front, name, marker):
        for cols in (120, 48):
            front.resize(30, cols)
            front.until(lambda shot: shot["size"] == [30, cols] and marker in text(shot),
                        "actual native result at the requested terminal width")
            self.save_shot(f"zcalld-{name}-{cols}", front, cells=True)
            self.frames.append({"name": name, "cols": cols, "rows": 30,
                                "path": f"input-history-zcalld-{name}-{cols}.json"})
        front.resize(30, 120)
        front.until(lambda shot: shot["size"] == [30, 120], "restored wide owned terminal")

    def endpoint(self, front):
        deadline = time.monotonic() + 6
        while time.monotonic() < deadline:
            endpoint = self.listener("status").get("local_endpoint")
            if endpoint:
                return Path(endpoint)
            front.pump(0.03)
        raise AssertionError("owned SSMCP client has no local_endpoint")

    def history_path(self):
        current = [item for item in self.observe("ssmcp.ui.history", {"action": "list"})["sessions"]
                   if item["current"]]
        self.check(len(current) == 1, "exactly one local JSONL conversation is current")
        return current[0]["id"], self.state_home / "ssmcp" / "conversations" / (current[0]["id"] + ".jsonl")

    def history_messages(self, path):
        return [item for item in map(json.loads, path.read_text().splitlines()) if item["type"] == "message"]

    def send_native(self, front, native, body, plan):
        self.draft(front, "/send " + body)
        front.send(ENTER)
        accepted = native.event("accepted", front)
        self.check(accepted["status"] == "accepted" and accepted["connection_id"] == self.client_id,
                   "native ACK confirms receipt on the exact currently selected connection")
        self.receipt(front, accepted["delivery_id"], "accepted")
        self.wait_state(front, lambda state: not state["draft"]["text"] and not state["force_pending"],
                        "accepted explicit /send clears only its matching draft")
        deadline = time.monotonic() + 5
        while not plan.started.is_set() and time.monotonic() < deadline:
            front.pump(0.03)
        self.check(plan.started.is_set() and not plan.release.is_set(),
                   "provider receives the request only after ACK, while its result remains withheld")
        self.check(plan.request["body"]["model"] == "fixture-upstream-model"
                   and plan.request["body"]["messages"] == [{"role": "user", "content": body}],
                   "real gateway routing preserves the complete user body and rewrites only the route")
        self.check(not plan.request["caller_credential_forwarded"],
                   "gateway caller credentials are not forwarded to the fake provider")
        return accepted

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        subprocess.run(["/usr/bin/git", "-c", "init.defaultBranch=main", "init", "-q"],
                       cwd=self.workspace, env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5)
        front = self.start_front("zcalld-native")
        front.resize(30, 120)
        front.until(lambda shot: shot["size"] == [30, 120], "owned native listener terminal")
        state = self.ui("/display/conversation")
        self.check(state["target"] is None and state["native_recipient"] is None,
                   "the real composer starts on the explicit Next caller target")
        ordinary = self.submit_source(front, "P1 ordinary Enter remains queued")
        endpoint = self.endpoint(front)
        denied = self.listener_error("open", label="remote cannot enable results", enable_reply=True)
        self.check("listener_reply_local_only" in denied,
                   "authenticated MCP cannot enable the private local reply capability")

        self.provider = Provider()
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            gateway_port = reservation.getsockname()[1]
        gateway_directory = self.directory / "gateway"
        gateway_directory.mkdir(mode=0o700)
        gateway_socket = gateway_directory / "gateway.sock"
        self.native_environment = self.client_environment("zcalld-native-process")
        self.native_environment.update({
            "P1_GATEWAY_TOKEN": "fixture-p1-gateway-" + self.directory.name,
            "NO_PROXY": "127.0.0.1,localhost,::1", "no_proxy": "127.0.0.1,localhost,::1",
        })
        self.sensitive.append(self.native_environment["P1_GATEWAY_TOKEN"])
        common = (
            f"[server]\nlisten=127.0.0.1:{gateway_port}\n"
            "trust_loopback=false\ntoken_env=P1_GATEWAY_TOKEN\nworkers=2\nqueue=4\n"
            "upstream_timeout_ms=22000\nconnect_timeout_ms=2000\n"
            "max_body_bytes=1048576\nmax_response_bytes=1048576\n"
        )
        provider = (
            f"\n[provider fake]\nprotocol=chat\nbase_url=http://127.0.0.1:{self.provider.server.server_port}/v1\nauth=none\n"
            "\n[route p1-route]\nprovider=fake\nmodel=fixture-upstream-model\n"
        )
        unix_config, tcp_config = gateway_directory / "unix.conf", gateway_directory / "tcp.conf"
        unix_config.write_text(common + f"unix_socket={gateway_socket}\n" + provider)
        tcp_config.write_text(common + "unix_socket=none\n" + provider)
        unix_config.chmod(0o600)
        tcp_config.chmod(0o600)
        gateway_log = (self.output / "zcalld-serve.log").open("wb")
        gateway = subprocess.Popen([str(self.zcalld), "serve", "--config", str(unix_config)],
                                   cwd=self.workspace, env=self.native_environment,
                                   stdin=subprocess.DEVNULL, stdout=gateway_log, stderr=subprocess.STDOUT,
                                   start_new_session=True)
        self.processes.append(gateway)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            try:
                connection = http.client.HTTPConnection("127.0.0.1", gateway_port, timeout=0.3)
                connection.request("GET", "/healthz")
                response = connection.getresponse()
                ready = response.status == 200
                response.read()
                connection.close()
                if ready and gateway_socket.exists():
                    break
            except OSError:
                pass
            front.pump(0.03)
        else:
            raise AssertionError("owned zcalld serve did not become ready")

        body = "  P1_INPUT_中文\n    printf \"literal $(do-not-run)\"\nsecond line  "
        result_text = "# P1_NATIVE_RESULT\n\n模型结果回到原会话。\n  spaces preserved"
        plan = self.provider.plan(result_text)
        native = Native(self, endpoint, unix_config, "native-unix")
        bound = native.event("bound", front)
        self.check(bound["connection_id"] == self.client_id and not self.provider.requests,
                   "opening native listen does not invoke a model or consume ordinary Enter input")
        self.ordinary_unchanged(ordinary)
        accepted = self.send_native(front, native, body, plan)
        self.check(not any(event.get("event") == "result" for event in native.events),
                   "accepted receipt is observable before model completion")
        busy = "/send P1_BUSY_INPUT must remain editable"
        self.draft(front, busy)
        front.send(ENTER)
        rejected = native.event("busy", front)
        self.receipt(front, rejected["delivery_id"], "rejected")
        self.check(len(self.provider.requests) == 1 and self.ui("/display/conversation")["draft"]["text"] == busy,
                   "a busy native receiver rejects the second send without another model call or lost draft")
        plan.release.set()
        result = native.event("result", front)
        native.exited(True)
        self.check(result["delivery_id"] == accepted["delivery_id"] and result["execution_state"] == "completed"
                   and result["history_saved"] and result["shown"],
                   "native result has the stable original delivery ID and a committed visible history receipt")
        conversation, path = self.history_path()
        messages = self.history_messages(path)
        inputs = [i for i, item in enumerate(messages) if item["role"] == "user" and item["content"] == body]
        replies = [(i, item) for i, item in enumerate(messages) if item["id"] == result["message_id"]]
        self.check(result["conversation_id"] == conversation and len(inputs) == len(replies) == 1
                   and inputs[0] < replies[0][0] and replies[0][1]["content"] == result_text,
                   "original input precedes exactly one native result in the original JSONL conversation")
        self.check(self.ui("/display/conversation")["draft"]["text"] == busy,
                   "late output does not clear a separately rejected editable draft")
        self.capture(front, "complete", "P1_NATIVE_RESULT")
        self.ordinary_unchanged(ordinary)
        self.evidence.append({"scenario": "unix-complete-busy", "accepted": accepted, "result": result})
        self.draft(front, "")

        plan = self.provider.plan("DO_NOT_COPY_PRIVATE_PROVIDER_ERROR", status=500)
        native = Native(self, endpoint, tcp_config, "native-tcp-failed")
        native.event("bound", front)
        accepted = self.send_native(front, native, "P1_PROVIDER_FAILURE", plan)
        plan.release.set()
        result = native.event("result", front)
        native.exited(True)
        self.check(result["execution_state"] == "failed"
                   and self.listener("status")["receipts"][-1]["status"] == "accepted",
                   "generation failure is separate from the already accepted input")
        visible = front.until(lambda shot: "HTTP 500" in text(shot), "bounded failed-generation result")
        self.check("DO_NOT_COPY_PRIVATE_PROVIDER_ERROR" not in text(visible)
                   and "DO_NOT_COPY_PRIVATE_PROVIDER_ERROR" not in path.read_text(),
                   "upstream error bodies are not copied into terminal or history")
        self.evidence.append({"scenario": "tcp-generation-failed", "accepted": accepted, "result": result})

        plan = self.provider.plan("P1_ARCHIVED_LATE_RESULT")
        native = Native(self, endpoint, unix_config, "native-archived")
        native.event("bound", front)
        self.send_native(front, native, "P1_INPUT_BEFORE_ARCHIVE", plan)
        front.send("/archive\r")
        front.until(lambda shot: "Conversation archived" in text(shot), "explicit archive of the source conversation")
        archived = path.read_bytes()
        plan.release.set()
        native.exited(False)
        self.check(path.read_bytes() == archived and not any(event.get("event") == "result" for event in native.events),
                   "archiving rejects late native output without modifying the old history")
        self.ordinary_unchanged(ordinary)

        plan = self.provider.plan("P1_DISCONNECTED_LATE_RESULT")
        native = Native(self, endpoint, unix_config, "native-disconnected")
        native.event("bound", front)
        self.send_native(front, native, "P1_INPUT_BEFORE_DISCONNECT", plan)
        request_count = len(self.provider.requests)
        old_client = self.client_id
        self.account_gateway.disconnect_client(old_client)
        credentials = self.new_credentials(front)
        self.client_id = credentials["client_id"]
        front.client_id, front.credentials = self.client_id, credentials
        self.check(self.client_id != old_client, "the owned relay reconnect creates a new actual connection identity")
        native.exited(False)
        plan.release.set()
        status = self.listener("status")
        self.check(status["listener"] is None and status["pending"] is None and status["receipts"] == [],
                   "new connection restores no old lease, forced delivery or receipt")
        self.check(len(self.provider.requests) == request_count
                   and not any(event.get("event") == "result" for event in native.events),
                   "disconnect triggers no second generation and publishes no zombie result")
        self.check("P1_DISCONNECTED_LATE_RESULT" not in "\n".join(p.read_text() for p in
                   (self.state_home / "ssmcp" / "conversations").glob("conversation_*.jsonl")),
                   "the disconnected result is absent from every retained conversation")

        endpoint = self.endpoint(front)
        program = Program(self, "unix", {"endpoint": str(endpoint)})
        opened = program.call("open", label="P1 duplicate-result fixture", enable_reply=True)
        self.check(opened["ok"], "an explicit local fixture opens a new reply-enabled lease")
        lease = {key: opened["result"][key] for key in ("listener_id", "token")}
        self.sensitive.append(lease["token"])
        delivery_id = self.force(front, "/send P1_DUPLICATE_SOURCE")
        offered = program.call("read", **lease, wait_ms=0)
        self.check(offered.get("ok") is True
                   and offered["result"]["delivery"]["delivery_id"] == delivery_id,
                   "the real local reader receives the exact duplicate-test delivery")
        early = program.call("reply", **lease, delivery_id=delivery_id, execution_state="completed", text="P1_DUPLICATE_RESULT")
        self.check(early.get("ok") is False and "listener_reply_expired" in early.get("error", ""),
                   "a pre-ACK result is rejected by the listener protocol, not by a worker failure")
        acknowledged = program.call("ack", **lease, delivery_id=delivery_id, status="accepted")
        self.check(acknowledged.get("ok") is True
                   and acknowledged["result"]["receipt"]["delivery_id"] == delivery_id
                   and acknowledged["result"]["receipt"]["status"] == "accepted",
                   "the real local ACK confirms the exact input before its result")
        result = program.call("reply", **lease, delivery_id=delivery_id, execution_state="completed", text="P1_DUPLICATE_RESULT")
        repeated = program.call("reply", **lease, delivery_id=delivery_id, execution_state="completed", text="P1_DUPLICATE_RESULT")
        conflict = program.call("reply", **lease, delivery_id=delivery_id, execution_state="completed", text="different")
        self.check(result.get("ok") is True and repeated.get("ok") is True
                   and result["result"]["duplicate"] is False and repeated["result"]["duplicate"] is True
                   and result["result"]["reply"] == repeated["result"]["reply"]
                   and conflict.get("ok") is False and "listener_reply_conflict" in conflict.get("error", ""),
                   "real Unix reply retries return one stable receipt while changed content conflicts")
        self.receipt(front, delivery_id, "accepted")
        _, current = self.history_path()
        message_id = result["result"]["reply"]["message_id"]
        self.check(sum(item["id"] == message_id for item in self.history_messages(current)) == 1,
                   "real duplicate result requests create exactly one history message")
        closed = program.call("close", **lease)
        self.check(closed.get("ok") is True and closed["result"]["closed"] is True,
                   "the actual local lease closes successfully before testing stale credentials")
        stale = program.call("reply", **lease, delivery_id=delivery_id, execution_state="completed", text="P1_DUPLICATE_RESULT")
        self.check(stale.get("ok") is False and "listener lease has expired" in stale.get("error", ""),
                   "closed local leases reject old reply credentials with an explicit lease error")
        program.close()
        self.evidence.append({"scenario": "real-reply-idempotency", "result": result["result"]["reply"]})
        self.check(len(self.provider.requests) == 4
                   and all(not request["unexpected"] for request in self.provider.requests),
                   "the provider saw exactly four planned HTTP requests and no hidden replay")
        gateway_log.close()
        return {"status": "passed", "provider_requests": self.provider.requests,
                "receipts": self.evidence, "frames": self.frames}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    parser.add_argument("--zcalld", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--zcalld-revision", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if not all(re.fullmatch(r"[0-9a-f]{40}", value) for value in (args.source_revision, args.zcalld_revision)):
        parser.error("both source revisions must be full immutable commit SHAs")
    binaries = {"ssmcp": args.bin_dir / "ssmcp", "ssmcpd": args.bin_dir / "ssmcpd",
                "decoder": args.decoder, "fixture": args.fixture_binary, "zcalld": args.zcalld}
    binaries = {name: path.resolve() for name, path in binaries.items()}
    if any(not path.is_file() or not os.access(path, os.X_OK) for path in binaries.values()):
        parser.error("all required executables must already exist; no implicit builds")
    args.out.mkdir(parents=True, exist_ok=True)
    if (args.out / "report.json").exists():
        parser.error("use a fresh output directory")
    before = {name: digest(path) for name, path in binaries.items()}
    # Imports reuse the real account/PTY harness. Record them as inputs, too;
    # the caller binds declared revisions to these hashes via its build manifest.
    harness_files = sorted(Path(__file__).resolve().parent.glob("e2e*.py"))
    harness_before = {path.name: digest(path) for path in harness_files}
    os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(binaries["fixture"])
    def interrupted(_signal, _frame):
        raise TimeoutError("native listener fixture interrupted")
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    started = time.monotonic()
    report = {
        "status": "failed", "source_revision": args.source_revision,
        "zcalld_revision": args.zcalld_revision,
        "revision_binding": "Caller-provided revisions; verify recorded hashes against immutable build manifests.",
    }
    # Keep owned Unix endpoints below sockaddr_un's path limit even when the
    # immutable build/report directory has a long provenance-rich name.
    with tempfile.TemporaryDirectory(prefix="ssmcp-zcl-", dir="/tmp") as directory:
        harness = Integration(Path(directory), args.bin_dir.resolve(), binaries["decoder"], args.out, binaries["zcalld"])
        def redact(value):
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    value = value.replace(secret, "[redacted]")
            return value
        try:
            report.update(harness.run())
        except Exception as error:
            report["error"] = redact(f"{type(error).__name__}: {error}")
            report["traceback"] = redact(traceback.format_exc())
            harness.diagnostics()
        finally:
            if harness.provider:
                harness.provider.close()
            harness.close_frontends()
            harness.cleanup()
        after = {name: digest(path) for name, path in binaries.items()}
        harness_after = {path.name: digest(path) for path in harness_files}
        unchanged = before == after and harness_before == harness_after
        report.update(checks=harness.checks, elapsed_seconds=round(time.monotonic() - started, 3),
                      binaries_sha256_before=before, binaries_sha256_after=after,
                      harness_sha256_before=harness_before, harness_sha256_after=harness_after,
                      inputs_unchanged=unchanged, script_sha256=digest(__file__))
        if not unchanged:
            report["status"], report["error"] = "failed", "an executable or harness changed during acceptance"
        encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        if redact(encoded) != encoded:
            raise AssertionError("report contains a private fixture credential")
        (args.out / "report.json").write_text(encoded, encoding="utf-8")
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]), "error": report.get("error")}))
    raise SystemExit(report["status"] != "passed")


if __name__ == "__main__":
    main()
