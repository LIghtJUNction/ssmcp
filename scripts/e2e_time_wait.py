#!/usr/bin/env python3
"""Verify native timers and live wait clocks through owned PTYs and loopback.

Uses prebuilt frozen product, router-fixture and VT-decoder binaries. The caller
must bind them to its build manifest and run in a private network namespace.
No Cargo, real model, existing client, account, service or wall clock is changed.
"""
import argparse
from concurrent.futures import ThreadPoolExecutor
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time

from e2e_client_ui import prompt_text
from e2e_composer import ENTER, LEFT
from e2e_dashboard import text
from e2e_interactions import Interactions, digest

ROWS = 32
DRAFT = "OWNED_TIMER_DRAFT"
CLOCK = re.compile(r"\b(Timer|Wait) (\+?)(\d+(?::\d{2}){1,2})")


def clock(shot):
    # This fixture fixes the real chat layout at 32 rows with three composer
    # rows. conversation::chat_frame draws separator_right at rows - 5. The
    # following three rows are editable input and the last row is a notice;
    # neither those nor preceding body text can establish a live clock.
    if shot.get("size", [None])[0] != ROWS or len(shot.get("rows", [])) != ROWS:
        return None
    row_index = ROWS - 5
    row = shot["rows"][row_index]
    divider = shot["rows"][0].find("│")
    header = row if divider < 0 else row[:divider]
    prompt = shot["rows"][row_index + 1]
    if divider >= 0:
        prompt = prompt[:divider]
    found = CLOCK.search(header) if prompt.lstrip().startswith("❯") else None
    if (not found or not header[:found.start()].endswith("─")
            or header[found.end():].strip()):
        return None
    parts = [int(part) for part in found[3].split(":")]
    seconds = parts[-1] + parts[-2] * 60 + (parts[0] * 3600 if len(parts) == 3 else 0)
    return {"kind": found[1], "elapsed": found[2] == "+", "seconds": seconds,
            "text": found[0], "row": row, "row_index": row_index,
            "surface": "composer_header"}


class TimeWait(Interactions):
    def __init__(self, directory, binary_dir, decoder, output, font):
        super().__init__(directory, binary_dir, decoder, output)
        self.font = font
        self.http_timeout = 35
        self.workers = ThreadPoolExecutor(max_workers=2)
        self.futures = []
        self.frame_manifest = []
        self.receipts = []
        self.await_receipts = []
        self.job_id = None
        self.metrics = {}

    def native(self, name, arguments, *, agent=None):
        response = self.response("return await call(%s, %s, %s);" % (
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments)), agent=agent)
        value = response["structuredContent"]["result"]
        if not isinstance(value, dict) or value.get("isError") or not isinstance(value.get("structuredContent"), dict):
            raise AssertionError("native timing fixture operation failed: " + name)
        return response, value["structuredContent"]

    def timer(self, arguments, *, agent=None):
        response, value = self.native("ssmcp.time.wait", arguments, agent=agent)
        self.check(value.get("schema_version") == 1 and value.get("completion_permission") is False,
                   "a native timer receipt is typed and never grants completion permission")
        for key in ("user_messages", "native_messages", "question_answers"):
            self.check(not response.get("structuredContent", {}).get(key),
                       "time-only calls do not consume " + key)
        self.receipts.append(value)
        return value

    def background(self, name, arguments):
        client_id = self.client_id
        code = """try {
    const native = await call(%s, %s, %s);
    return {ok:native.isError !== true,native};
} catch (error) { return {ok:false,error:String(error.message ?? error)}; }""" % (
            json.dumps(client_id), json.dumps(name), json.dumps(arguments))
        future = self.workers.submit(self.response, code)
        self.futures.append(future)
        return future

    def finish_future(self, front, future):
        deadline = time.monotonic() + 8
        while not future.done() and time.monotonic() < deadline:
            front.pump(0.03)
        if not future.done():
            raise AssertionError("cancelled owned observation did not return its HTTP receipt")
        return future.result()["structuredContent"]["result"]

    def public_wait(self, arguments, member):
        arguments = dict(arguments, client_id=self.client_id,
                         agent={"id":member["agent_id"], "token":member["token"]})
        future = self.workers.submit(self.public_call, "await", arguments)
        self.futures.append(future)
        return future

    def finish_public_wait(self, front, future):
        deadline = time.monotonic() + 8
        while not future.done() and time.monotonic() < deadline:
            front.pump(0.03)
        if not future.done():
            raise AssertionError("the owned public await did not finish within its bounded observation")
        value = future.result()
        self.check(value.get("schema_version") == 1 and value.get("completion_permission") is None,
                   "a public wait clock does not create task-completion permission")
        for key in ("user_messages", "native_messages", "question_answers", "tool_summary_reminders"):
            self.check(not value.get(key), "the public wait clock does not consume or inject " + key)
        self.await_receipts.append(value)
        return value

    def wait_timing(self, front, predicate):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.03)
            value = self.ui("/time")
            found = [entry for entry in value["active"] if predicate(entry)]
            if len(found) == 1:
                return found[0]
        raise AssertionError("the exact owned timing entry did not reach the live UI")

    def local(self, front, command):
        self.replace_short(front, command)
        front.send(ENTER)
        self.wait_state(front, lambda item: item["draft"]["text"] == "",
                        "the local wait command completes without publishing a user request")

    def verify_clock_layout(self):
        display = self.ui("/display")
        pane = display["panes"]["input"]
        self.check(pane["row"] == ROWS - 4 and pane["rows"] == 3 and pane["visible"] is True,
                   "the actual composer geometry pins clock header 27 and excludes body, draft and footer")

    def capture_clock(self, front, name, kind, elapsed):
        renderer = Path(__file__).with_name("render_ui_snapshot.py")
        for width in (120, 48):
            front.resize(ROWS, width)
            front.pump(0.1)
            self.verify_clock_layout()
            shot = front.until(lambda value: value["size"] == [ROWS, width]
                               and (seen := clock(value)) is not None
                               and seen["kind"] == kind and seen["elapsed"] is elapsed
                               and self.saved_draft["text"] in prompt_text(value),
                               name + " has the real clock and draft at " + str(width), cells=True)
            stem = "time-wait-" + name + "-" + str(width)
            encoded = json.dumps(shot, ensure_ascii=False, indent=2) + "\n"
            if any(secret and secret in encoded for secret in [self.secret, *self.sensitive]):
                raise AssertionError("a fixture credential reached the physical clock frame")
            snapshot = self.output / (stem + ".json")
            snapshot.write_text(encoded, encoding="utf-8")
            (self.output / (stem + ".txt")).write_text(text(shot) + "\n", encoding="utf-8")
            png = self.output / (stem + ".png")
            subprocess.run([sys.executable, str(renderer), str(snapshot), str(png), "--font", str(self.font)],
                           check=True, capture_output=True, text=True, timeout=20)
            self.frames.append(png.name)
            self.frame_manifest.append({"path": png.name, "sha256": digest(png), "cells": snapshot.name,
                                        "cells_sha256": digest(snapshot), "size": shot["size"], "clock": clock(shot)})
            self.check(self.ui("/display/conversation")["draft"] == self.saved_draft,
                       name + ": exact draft bytes and cursor survive " + str(width) + "-column rendering")
        front.resize(ROWS, 120)
        front.pump(0.1)

    def preserve_queued(self, original):
        current = self.record(self.queue(), message_id=original["id"])
        self.check(current["status"] == "queued" and current["text"] == original["text"]
                   and current["revision"] == original["revision"] and current["target"] == original["target"],
                   "the exact ordinary requirement stays queued with its original revision and target")

    def save_draft(self, front, value=DRAFT):
        self.replace_short(front, value)
        front.send(LEFT)
        state = self.wait_state(front, lambda item: item["draft"]["text"] == value
                                and item["draft"]["cursor"] == len(value.encode()) - 1,
                                "the owned timing draft has its exact nonterminal cursor")
        self.saved_draft = state["draft"]

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        front = self.start_front("PTY-time-wait")
        member = self.public_call("collab", {"action": "join", "client_id": self.client_id,
                                            "name": "time scope fixture", "role": "fixture only"})
        self.sensitive.append(member["token"])
        discovered = self.response("return await tools.search(%s,{name_prefix:'ssmcp.time.',include_schemas:true});" % json.dumps(self.client_id))
        advertised = discovered["structuredContent"]["result"]
        self.check(advertised.get("complete") is True
                   and {tool["name"] for tool in advertised["tools"]} == {"ssmcp.time.now", "ssmcp.time.wait"},
                   "the real native catalog advertises exactly both time primitives")
        queued = self.submit_source(front, "TIME_REQUIREMENT_MUST_STAY_QUEUED")
        self.save_draft(front)
        self.verify_clock_layout()
        response, now = self.native("ssmcp.time.now", {})
        self.check(isinstance(now.get("unix_ms"), int) and now["unix_ms"] > 1_000_000_000_000
                   and isinstance(now.get("monotonic_ms"), int) and now["deadline_clock"] == "monotonic"
                   and now["wall_clock"] == "UTC Unix epoch" and not self.messages(response),
                   "time.now separates wall-clock readout from its monotonic counter without reading input")
        timer = self.timer({"duration_ms":60_000, "label":"Owned countdown"})
        self.check(timer["status"] == "running" and 0 < timer["remaining_ms"] <= 60_000,
                   "a start returns the stable background timer ID immediately")
        first = self.quiet_until(front, lambda shot: (value := clock(shot)) is not None
                                 and value["kind"] == "Timer" and not value["elapsed"],
                                 "the countdown appears without an extra keyboard or UI poll")
        before = clock(first)["seconds"]
        self.quiet_until(front, lambda shot: (value := clock(shot)) is not None
                         and not value["elapsed"] and value["seconds"] < before,
                         "the countdown advances on the compositor's own second tick", timeout=4)
        self.capture_clock(front, "countdown", "Timer", False)
        same = self.timer({"op":"status", "timer_id":timer["timer_id"], "wait_ms":20})
        self.check(same["timer_id"] == timer["timer_id"] and same["status"] == "running"
                   and same["wait_timed_out"] is True and same["remaining_ms"] < timer["remaining_ms"],
                   "a bounded observation timeout leaves the same monotonic timer running")
        self.preserve_queued(queued)

        # A legal binding cannot acquire an unbound timer or make a time-only
        # run eligible to take the ordinary queue as a side effect.
        code = """try { const native = await call(%s, 'ssmcp.time.wait', %s);
return {rejected:native.isError === true,reason:native.content?.find(item=>item.type==='text')?.text ?? ''};
} catch(error) { return {rejected:true,reason:String(error.message ?? error)}; }""" % (
            json.dumps(self.client_id), json.dumps({"op":"cancel", "timer_id":timer["timer_id"]}))
        rejected = self.response(code, agent=self.binding(member))
        result = rejected["structuredContent"]["result"]
        self.check(result["rejected"] is True and "belongs to another caller" in result["reason"]
                   and not self.messages(rejected),
                   "a different authenticated participant cannot cancel the original timer or consume input")
        self.preserve_queued(queued)

        changed = self.saved_draft["text"][:-1] + "z" + self.saved_draft["text"][-1:]
        started = time.monotonic()
        front.send("z")
        front.until(lambda shot: changed in prompt_text(shot), "typing remains responsive during countdown", timeout=2)
        latency = (time.monotonic() - started) * 1000
        self.metrics["physical_key_to_frame_ms"] = round(latency, 3)
        self.check(latency <= 500, "the actual input-to-frame latency stays within 500ms while counting down")
        self.local(front, "/wait cancel " + timer["timer_id"])
        stopped = self.timer({"op":"status", "timer_id":timer["timer_id"]})
        self.check(stopped["status"] == "cancelled", "a real PTY /wait cancel stops only the exact timer")
        self.preserve_queued(queued)
        self.save_draft(front)

        elapsed = self.timer({"label":"Owned elapsed timer"})
        self.check(elapsed["remaining_ms"] is None, "an unknown end has no fabricated remaining deadline")
        shot = self.quiet_until(front, lambda shot: (value := clock(shot)) is not None and value["elapsed"],
                                "the unknown-end timer displays elapsed time")
        initial = clock(shot)["seconds"]
        self.quiet_until(front, lambda shot: (value := clock(shot)) is not None
                         and value["elapsed"] and value["seconds"] > initial,
                         "elapsed time advances without tool polling or keyboard activity", timeout=4)
        self.capture_clock(front, "elapsed", "Timer", True)
        self.timer({"op":"cancel", "timer_id":elapsed["timer_id"]})
        self.preserve_queued(queued)

        # Real work restores its ordinary delivery policy even when the same
        # run also uses the time primitive. The job is owned by this fixture.
        code = """await call(%s,'ssmcp.time.now',{});
return await call(%s,'ssmcp.exec.start',%s);""" % (
            json.dumps(self.client_id), json.dumps(self.client_id),
            json.dumps({"command":[sys.executable,"-c","import time; time.sleep(60)"],
                        "cwd":str(self.workspace),"title":"Owned timer cancellation fixture"}))
        response = self.response(code)
        self.expect_delivery(response, queued)
        native = response["structuredContent"]["result"]
        self.check(not native.get("isError") and native["structuredContent"]["running"] is True,
                   "the owned background job starts through the actual execution tool")
        self.job_id = native["structuredContent"]["job_id"]
        future = self.background("ssmcp.exec.wait", {"job_id":self.job_id, "wait_ms":25_000})
        observer = self.wait_timing(front, lambda entry: entry["kind"] == "observation"
                                    and entry["label"] == "ssmcp.exec.wait")
        self.check(observer["cancellable"] is True and 0 < observer["remaining_ms"] <= 25_000,
                   "the real native wait has a bounded, independently cancellable countdown")
        self.capture_clock(front, "native-observer", "Wait", False)
        self.local(front, "/wait cancel " + observer["id"])
        cancelled = self.finish_future(front, future)
        self.check(cancelled["ok"] is False and "native wait observation cancelled" in cancelled.get("error", ""),
                   "local cancellation returns the exact observation cancellation, not a transport error")
        _, status = self.native("ssmcp.exec.status", {"job_id":self.job_id})
        self.check(status["running"] is True and status.get("exit_code") is None,
                   "cancelling the observer leaves its real owned job running")
        self.native("ssmcp.exec.cancel", {"job_id":self.job_id})
        self.job_id = None
        self.save_draft(front)

        # The existing user-wait UI gets a live countdown without acquiring an
        # input reservation or producing any task-completion permission here.
        wait_id = "timer_fixture_user_wait"
        future = self.background("ssmcp.ui.wait", {"op":"wait", "wait_id":wait_id,
            "episode_id":"a" * 64, "intent":"need_user", "recipient":"next",
            "reason":"Owned countdown display fixture", "timeout_ms":20_000, "notify":False})
        self.quiet_until(front, lambda shot: (seen := clock(shot)) is not None and seen["kind"] == "Wait",
                         "the existing user wait displays its native remaining budget")
        self.capture_clock(front, "user-wait", "Wait", False)
        self.local(front, "/wait cancel " + wait_id)
        completed = self.finish_future(front, future)
        self.check(completed["ok"] is True and completed["native"]["structuredContent"]["status"] == "cancelled",
                   "the real user-wait cancellation remains a cancellation, never a decision")
        self.save_draft(front)

        # This public Rust await executes no native operation. Its display
        # arrives through the negotiated, independent relay snapshot channel.
        # The known participant starts at `available`; readiness is changed
        # explicitly only after the real countdown has been observed.
        self.verify_clock_layout()
        public_queued = self.submit_source(front, "PUBLIC_WAIT_REQUIREMENT_STAYS_QUEUED")
        self.save_draft(front)
        self.verify_clock_layout()
        target = {"type":"agent_status", "agent_id":member["agent_id"],
                  "statuses":["timer-fixture-ready"]}
        future = self.public_wait({"targets":[target], "timeout_ms":4000}, member)
        first = self.wait_timing(front, lambda entry: entry["kind"] == "relay_wait")
        self.check(first["attribution"]["agent"]["id"] == member["agent_id"]
                   and first["cancellable"] is False and first["episode"] > 0,
                   "the server-only clock names its actual participant and grants no local action")
        first_shot = self.quiet_until(front, lambda shot: (seen := clock(shot)) is not None
                                     and seen["kind"] == "Wait" and not seen["elapsed"],
                                     "the public await countdown reaches the real composer header")
        first_seconds = clock(first_shot)["seconds"]
        self.quiet_until(front, lambda shot: (seen := clock(shot)) is not None
                         and seen["kind"] == "Wait" and seen["seconds"] < first_seconds,
                         "a server-only await advances without native polling or keyboard input", timeout=3)
        timed_out = self.finish_public_wait(front, future)
        self.check(timed_out["status"] == "timed_out" and timed_out["wait_id"] == first["id"],
                   "public observation timeout retains the server-issued wait ID")
        self.preserve_queued(public_queued)

        self.verify_clock_layout()
        future = self.public_wait({"wait_id":first["id"], "timeout_ms":25_000}, member)
        resumed = self.wait_timing(front, lambda entry: entry["kind"] == "relay_wait"
                                   and entry["id"] == first["id"] and entry["episode"] > first["episode"])
        self.capture_clock(front, "public-await", "Wait", False)
        self.preserve_queued(public_queued)
        status, updated_rpc, _ = self.rpc("tools/call", {"name":"collab", "arguments":{
            "action":"update", "client_id":self.client_id, "agent_id":member["agent_id"],
            "token":member["token"], "status":"timer-fixture-ready"}}, modern=True)
        if status != 200 or not isinstance(updated_rpc, dict) or "error" in updated_rpc:
            raise AssertionError("explicit collaboration update failed at the HTTP boundary")
        updated = updated_rpc["result"]
        self.check(not updated.get("isError"), "the explicit collaboration status update succeeds")
        # The separate explicit collaboration update is ordinary accepted work;
        # its existing queue-delivery semantics must remain unchanged.
        # Keep the real MCP content block as well as structuredContent so the
        # shared receipt check can verify the visible requirement and its scope.
        self.expect_delivery(updated, public_queued, agent_id=member["agent_id"])
        ready = self.finish_public_wait(front, future)
        self.check(ready["status"] == "ready" and ready["wait_id"] == resumed["id"]
                   and ready["results"][0]["data"]["matched"] is True,
                   "the same timed-out wait becomes ready only after its real target status changes")

        self.public_call("collab", {"action":"update", "client_id":self.client_id,
            "agent_id":member["agent_id"], "token":member["token"], "status":"available"})
        cancel_queued = self.submit_source(front, "PUBLIC_WAIT_CANCEL_STAYS_PASSIVE")
        self.save_draft(front)
        self.verify_clock_layout()
        future = self.public_wait({"targets":[target], "timeout_ms":25_000}, member)
        cancelled_entry = self.wait_timing(front, lambda entry: entry["kind"] == "relay_wait"
                                           and entry["id"] != first["id"])
        cancel = self.public_call("await", {"client_id":self.client_id,
            "agent":{"id":member["agent_id"], "token":member["token"]},
            "op":"cancel", "wait_id":cancelled_entry["id"]})
        self.check(cancel["status"] == "cancelled" and cancel.get("completion_permission") is None,
                   "explicit public cancellation ends only its named observation")
        cancelled = self.finish_public_wait(front, future)
        self.check(cancelled["status"] == "cancelled" and cancelled["wait_id"] == cancelled_entry["id"],
                   "the active public waiter receives the same cancellation without replay")
        self.preserve_queued(cancel_queued)
        self.quiet_until(front, lambda shot: clock(shot) is None,
                         "the stopped public observation clears its live header clock")

        pending = self.timer({"duration_ms":60_000, "label":"Disconnect clears this timer"})
        old_id = self.client_id
        self.account_gateway.disconnect_client(old_id)
        credentials = self.new_credentials(front)
        self.client_id = credentials["client_id"]
        front.client_id = self.client_id
        front.credentials = credentials
        self.check(self.client_id != old_id and self.ui("/time") == {"active":[],"recent":[]},
                   "a fresh connection restores no active timers, observations or completion ledger")
        self.check(self.ui("/display/conversation")["draft"] == self.saved_draft,
                   "disconnect/reconnect preserves the exact unsubmitted draft and cursor")
        _, fresh = self.native("ssmcp.time.now", {})
        self.check(fresh["monotonic_scope"] != now["monotonic_scope"],
                   "the replacement connection has a distinct monotonic clock scope")
        self.receipts.append({"disconnected_timer_id":pending["timer_id"], "replayed":False})
        for secret in [self.secret, *self.sensitive]:
            self.check(not secret or secret.encode() not in front.transcript,
                       "the physical terminal contains no private fixture credential")
        return {"status":"passed","checks":len(self.checks),"passed_checks":self.checks,
                "frames":self.frames,"frame_manifest":self.frame_manifest,"metrics":self.metrics,
                "timer_receipts":self.receipts,"quiet_checks":self.quiet_checks,
                "public_await_receipts":self.await_receipts,
                "clock_layout":{"rows":ROWS,"header_row":27,"input_row":28,"input_rows":3,
                                "surface":"composer_header","left_pane_only":True},
                "transport":"authenticated loopback router + native tools + owned OS PTY + independent VT decoder",
                "models_started":False,"real_user_credentials_used":False,"real_finish_permission":False,
                "wall_clock_changed":False,"existing_services_touched":False}

    def close_owned_waits(self):
        if self.job_id:
            try:
                self.native("ssmcp.exec.cancel", {"job_id":self.job_id})
            except Exception:
                pass
        # All blocking fixture calls have <=25s native budgets and a 35s HTTP
        # ceiling. Only these owned requests/processes are joined or cleaned up.
        self.workers.shutdown(wait=True, cancel_futures=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--font", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        parser.error("source revision must be the full frozen source SHA")
    binaries = {name: args.bin_dir.resolve() / name for name in ("ssmcp", "ssmcpd")}
    binaries.update(decoder=args.decoder.resolve(), fixture=args.fixture_binary.resolve())
    if any(not path.is_file() or not os.access(path, os.X_OK) for path in binaries.values()):
        parser.error("all immutable executables must already exist; this fixture never builds")
    if not args.font.is_file():
        parser.error("font must be an existing pinned monospace font")
    args.out.mkdir(parents=True, exist_ok=True)
    if any(args.out.iterdir()):
        parser.error("use an empty output directory; prior evidence is never overwritten")
    before = {name:digest(path) for name, path in binaries.items()}
    source_paths = sorted(Path(__file__).parent.glob("e2e_*.py")) + [Path(__file__).with_name("render_ui_snapshot.py")]
    source_before = {path.name:digest(path) for path in source_paths}
    font_before = digest(args.font)
    versions = {name:subprocess.run([str(binaries[name]),"--version"], check=True, capture_output=True, text=True, timeout=5).stdout.strip()
                for name in ("ssmcp","ssmcpd")}
    os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(binaries["fixture"])

    def interrupted(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    started = time.monotonic()
    report = {"status":"failed"}
    with tempfile.TemporaryDirectory(prefix="ssmcp-time-wait-") as temporary:
        harness = TimeWait(Path(temporary), args.bin_dir.resolve(), binaries["decoder"], args.out, args.font.resolve())
        try:
            report = harness.run()
        except Exception as error:
            detail = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    detail = detail.replace(secret, "[redacted]")
            report.update(error=detail, checks=len(harness.checks), passed_checks=harness.checks,
                          frames=harness.frames, frame_manifest=harness.frame_manifest)
            harness.diagnostics()
        finally:
            harness.close_owned_waits()
            harness.close_frontends()
            harness.cleanup()
        after = {name:digest(path) for name, path in binaries.items()}
        source_after = {path.name:digest(path) for path in source_paths}
        unchanged = before == after and source_before == source_after and font_before == digest(args.font)
        if not unchanged:
            report.update(status="failed",error="a frozen binary, source or font changed during the fixture")
        report.update(source_revision=args.source_revision,
                      source_revision_origin="caller-supplied; bind to the external passed build manifest",
                      duration_seconds=round(time.monotonic()-started,3), versions=versions,
                      binary_paths={name:str(path) for name,path in binaries.items()},
                      binaries_sha256_before=before,binaries_sha256_after=after,
                      source_sha256_before=source_before,source_sha256_after=source_after,
                      font_sha256=font_before,inputs_unchanged=unchanged)
        encoded = json.dumps(report,ensure_ascii=False,indent=2)+"\n"
        if any(secret and secret in encoded for secret in [harness.secret,*harness.sensitive]):
            raise AssertionError("the shareable timing report contains a private fixture secret")
        (args.out/"report.json").write_text(encoded,encoding="utf-8")
        print(json.dumps({"status":report["status"],"checks":report.get("checks",0),"frames":len(harness.frames),"error":report.get("error")}))
    if report["status"] != "passed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
