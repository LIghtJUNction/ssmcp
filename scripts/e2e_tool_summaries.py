#!/usr/bin/env python3
"""Verify tool-group summaries through an authenticated loopback relay and an owned PTY.

Requires prebuilt immutable client, server, router-fixture, and VT-decoder binaries.
No Cargo, external model, installed account, live session, desktop, or X server is used.
The PNGs render actual VT cells captured from the test's private physical terminal.
"""

import argparse
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time

from e2e_client_ui import body_text, prompt_text
from e2e_composer import LEFT
from e2e_dashboard import text
from e2e_interactions import Interactions, digest

TOOL = "ssmcp.ui.tool_summary"
DRAFT = "DRAFT_STAYS intact"
FIRST = "Inspecting workspace files"
SECOND = "Verified workspace files"


class ToolSummaries(Interactions):
    def __init__(self, directory, binary_dir, decoder, output, font):
        super().__init__(directory, binary_dir, decoder, output)
        self.font = font
        self.receipts = []
        self.metrics = {}

    def native_summary(self, arguments, *, agent=None, expected_error=None):
        invocation = "await call(%s, %s, %s)" % (
            json.dumps(self.client_id), json.dumps(TOOL), json.dumps(arguments))
        if expected_error:
            code = """try {
    const result = %s;
    const reason = result.content?.find(item => item.type === "text")?.text ?? "";
    return {rejected:result.isError === true,reason};
} catch (error) {
    return {rejected:true,reason:String(error.message ?? error)};
}""" % invocation
        else:
            code = "return %s;" % invocation
        response = self.response(code, agent=agent)
        native = response["structuredContent"]["result"]
        if expected_error:
            self.check(native.get("rejected") is True
                       and native.get("reason", "").startswith(expected_error + ":"),
                       "the native summary update rejects with " + expected_error)
            return response, native
        if native.get("isError") or not isinstance(native.get("structuredContent"), dict):
            raise AssertionError("the native summary operation failed")
        return response, native["structuredContent"]

    def groups(self):
        response, value = self.native_summary({"action": "list"})
        if value.get("schema_version") != 1 or not isinstance(value.get("groups"), list):
            raise AssertionError("unsupported tool-summary list schema")
        return response, value["groups"]

    def group(self, group_id):
        response, groups = self.groups()
        matches = [group for group in groups if group["group_id"] == group_id]
        if len(matches) != 1:
            raise AssertionError("the same authenticated group ID must remain uniquely readable")
        return response, matches[0]

    @staticmethod
    def reminders(response):
        return response.get("structuredContent", {}).get("tool_summary_reminders", [])

    def calls(self, count):
        code = """const id = %s;
const results = [];
for (let i = 0; i < %d; i++) {
    const result = await call(id, "ssmcp.fs.stat", {path:"summary-fixture.txt"});
    if (result.isError) throw new Error("owned stat fixture failed");
    results.push(result.structuredContent.kind);
}
return {count:results.length,kinds:results};""" % (json.dumps(self.client_id), count)
        response = self.response(code)
        result = response["structuredContent"]["result"]
        self.check(result == {"count": count, "kinds": ["file"] * count},
                   str(count) + " real native file-stat calls completed successfully")
        return response

    def update(self, group, summary):
        response, value = self.native_summary({
            "action": "set", "group_id": group["group_id"], "summary": summary,
            "expected_revision": group["revision"],
        })
        self.check(value.get("accepted") is True, "the summary tool returns an accepted update")
        _, current = self.group(group["group_id"])
        self.check(current["summary"] == summary
                   and current["revision"] == group["revision"] + 1
                   and current["call_count"] == group["call_count"]
                   and current["covered_call_count"] == current["call_count"],
                   "updating the same group advances only its summary revision and coverage")
        self.check(not self.reminders(response), "summary updates do not issue tool-count reminders")
        self.receipts.append({key: current[key] for key in (
            "group_id", "revision", "summary", "call_count", "covered_call_count")})
        return current

    def proactive_update(self, group):
        code = """const id = %s;
for (let i = 0; i < 11; i++) {
    const result = await call(id, "ssmcp.fs.stat", {path:"summary-fixture.txt"});
    if (result.isError) throw new Error("owned stat fixture failed");
}
const listed = await call(id, %s, {action:"list"});
if (listed.isError) throw new Error("summary list failed");
const group = listed.structuredContent.groups.find(group => group.group_id === %s);
if (!group) throw new Error("the original group disappeared");
return await call(id, %s, {
    action:"set",group_id:group.group_id,summary:"Tracking validation progress",
    expected_revision:group.revision
});""" % (json.dumps(self.client_id), json.dumps(TOOL),
          json.dumps(group["group_id"]), json.dumps(TOOL))
        response = self.response(code)
        native = response["structuredContent"]["result"]
        self.check(not native.get("isError") and native["structuredContent"].get("accepted") is True,
                   "one real code_mode run completes eleven calls and proactively updates that group")
        self.check(not self.reminders(response),
                   "a successful same-run update removes the older pending summary reminder")
        _, current = self.group(group["group_id"])
        self.check(current["call_count"] == group["call_count"] + 11
                   and current["covered_call_count"] == current["call_count"]
                   and current["revision"] == group["revision"] + 1,
                   "the proactive update covers all calls at the new revision")
        self.receipts.append({key: current[key] for key in (
            "group_id", "revision", "summary", "call_count", "covered_call_count")})
        return current

    def failed_calls(self):
        code = """const id = %s;
let failed = 0, unexpected = 0;
for (let i = 0; i < 11; i++) {
    try {
        const result = await call(id, "ssmcp.fs.stat", {path:"absent-summary-fixture"});
        if (result.isError) failed++; else unexpected++;
    } catch (_) { failed++; }
}
return {failed,unexpected};""" % json.dumps(self.client_id)
        started = time.monotonic()
        response = self.response(code)
        self.metrics["failed_call_batch_seconds"] = round(time.monotonic() - started, 3)
        self.check(response["structuredContent"]["result"] == {"failed": 11, "unexpected": 0},
                   "eleven actual missing-file calls fail without a mutation retry")
        return response

    def visible_group(self, group_id):
        events = self.ui("/display/events")
        matches = [item for item in events["tool_groups"] if item.get("group_id") == group_id]
        if len(matches) != 1:
            raise AssertionError("the real terminal must expose the same opaque group ID")
        return events, matches[0]

    def wait_group(self, front, group_id, predicate, description):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            events, group = self.visible_group(group_id)
            if predicate(group):
                return events, group
        raise AssertionError(description + "\n" + front.failure_detail())

    def history_messages(self):
        sessions = self.observe("ssmcp.ui.history", {"action": "list"})["sessions"]
        messages = []
        for session in sessions:
            page = self.observe("ssmcp.ui.history", {
                "action": "read", "id": session["id"], "limit": 64,
            })
            if page.get("next_offset") is not None:
                raise AssertionError("the bounded fixture unexpectedly has more than 64 history messages")
            messages.extend((session["id"], item["role"], item["content"])
                            for item in page["messages"]
                            if item["role"] in ("user", "assistant", "system"))
        return messages

    def mouse(self, front, marker, button):
        shot = front.until(lambda value: marker in body_text(value),
                           "the visible group header before pointer input")
        row = next(index for index, value in enumerate(shot["rows"]) if marker in value)
        front.send(("\x1b[<%d;9;%dM" % (button, row + 1)).encode())
        if button == 0:
            front.send(("\x1b[<0;9;%dm" % (row + 1)).encode())

    def frames_for(self, front, name, marker, count):
        renderer = Path(__file__).with_name("render_ui_snapshot.py")
        for columns in (120, 48):
            front.resize(32, columns)
            shot = front.until(
                lambda value: value["size"] == [32, columns]
                and marker in body_text(value)
                and re.search(r"\b" + str(count) + r" calls\b", body_text(value))
                and DRAFT in prompt_text(value),
                name + " summary, count, and retained input at " + str(columns) + " columns",
                cells=True)
            stem = "tool-summary-" + name + "-" + str(columns)
            encoded = json.dumps(shot, ensure_ascii=False, indent=2) + "\n"
            if any(secret and secret in encoded for secret in self.sensitive):
                raise AssertionError("a private fixture credential reached the captured terminal")
            snapshot = self.output / (stem + ".json")
            snapshot.write_text(encoded, encoding="utf-8")
            (self.output / (stem + ".txt")).write_text(text(shot) + "\n", encoding="utf-8")
            subprocess.run([
                sys.executable, str(renderer), str(snapshot),
                str(self.output / (stem + ".png")), "--font", str(self.font),
            ], check=True, capture_output=True, text=True, timeout=20)
            self.frames.append(stem + ".png")
            self.check(self.ui("/display/conversation")["draft"] == self.saved_draft,
                       name + ": the exact text and UTF-8 cursor survive " + str(columns) + "-column rendering")
        front.resize(32, 120)
        front.pump(0.15)

    def assert_reminder(self, response, group, count):
        reminders = self.reminders(response)
        matches = [item for item in reminders if item.get("group_id") == group["group_id"]]
        self.check(len(matches) == 1 and matches[0]["client_id"] == self.client_id
                   and matches[0]["call_count"] == count
                   and matches[0]["covered_call_count"] == group["covered_call_count"]
                   and matches[0]["revision"] == group["revision"]
                   and matches[0]["threshold"] == 10 and matches[0]["tool"] == TOOL,
                   "the outer code_mode response identifies the exact overdue group after eleven new calls")
        self.check(any("TOOL SUMMARY REMINDER" in block.get("text", "")
                       for block in response.get("content", []) if block.get("type") == "text"),
                   "the model-facing response also contains the explicit summary reminder text block")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        (self.workspace / "summary-fixture.txt").write_text("owned tool-summary fixture\n")
        front = self.start_front("PTY-tool-summaries")
        discovered = self.response("return await tools.search(%s, %s);" % (
            json.dumps(self.client_id),
            json.dumps({"name_prefix": TOOL, "include_schemas": True})))
        self.check(any(item.get("name") == TOOL
                       for item in discovered["structuredContent"]["result"]["tools"]),
                   "the live client advertises the summary tool before metadata is trusted")
        self.replace_short(front, DRAFT)
        front.send(LEFT * 3)
        state = self.wait_state(
            front, lambda item: item["draft"] == {"text": DRAFT, "cursor": len(DRAFT.encode()) - 3},
            "the synthetic draft with an interior cursor")
        self.saved_draft = state["draft"].copy()
        baseline_ids = {item["group_id"] for item in self.groups()[1]}

        ten = self.calls(10)
        self.check(not self.reminders(ten), "ten logical calls do not inject a summary reminder")
        _, listed = self.groups()
        groups = [group for group in listed if group["group_id"] not in baseline_ids]
        self.check(len(groups) == 1 and groups[0]["call_count"] == 10,
                   "ten adjacent native calls produce one stable group")
        group = groups[0]
        group_id = group["group_id"]
        _, same = self.group(group_id)
        self.check(same["call_count"] == 10 and same["revision"] == group["revision"],
                   "repeated list operations preserve group identity, count, and revision")

        eleven = self.calls(1)
        self.assert_reminder(eleven, group, 11)
        listed_response, group = self.group(group_id)
        self.check(group["call_count"] == 11 and not self.reminders(listed_response),
                   "listing does not count as work or repeat an already delivered reminder")
        messages_before = self.history_messages()
        first = self.update(group, FIRST)
        self.frames_for(front, "first-folded", FIRST, first["call_count"])
        # The tool viewport holds twelve rows. Create real overflow before
        # requiring a wheel gesture to move its internal scroll position.
        overflow = self.calls(2)
        self.check(not self.reminders(overflow),
                   "two calls after the first summary remain below the next reminder threshold")
        _, overflow_group = self.group(group_id)
        self.check(overflow_group["call_count"] == first["call_count"] + 2
                   and overflow_group["covered_call_count"] == first["covered_call_count"]
                   and overflow_group["call_count"] > 12,
                   "additional native calls overflow the twelve-row tool viewport")
        self.mouse(front, FIRST, 0)
        self.wait_group(front, group_id, lambda item: item["expanded"], "expand the actual tool group")
        self.mouse(front, FIRST, 65)
        events_before, visible_before = self.wait_group(
            front, group_id, lambda item: item["scroll"] > 0,
            "scroll inside the bounded tool group")
        self.check(self.ui("/display/conversation")["draft"] == self.saved_draft,
                   "expansion and internal scrolling retain the unsent draft and its cursor")
        second = self.update(overflow_group, SECOND)
        events_after, visible_after = self.wait_group(
            front, group_id, lambda item: item["summary"] == SECOND,
            "replace the existing visible group summary")
        self.check((visible_after["expanded"], visible_after["scroll"], events_after["scroll"])
                   == (visible_before["expanded"], visible_before["scroll"], events_before["scroll"]),
                   "summary replacement preserves group expansion, inner scroll, and outer scroll")
        self.check(events_after["retained_events"] == events_before["retained_events"]
                   and self.history_messages() == messages_before,
                   "two summary updates replace metadata without appending chat messages or tool records")
        self.frames_for(front, "updated-expanded", SECOND, second["call_count"])

        self.native_summary({
            "action": "set", "group_id": group_id, "summary": "stale overwrite",
            "expected_revision": first["revision"],
        }, expected_error="tool_summary_revision_conflict")
        peer = self.public_call("collab", {
            "action": "join", "client_id": self.client_id,
            "name": "isolated summary scope fixture", "role": "test-only",
        })
        self.sensitive.append(peer["token"])
        binding = {"client_id": self.client_id, "id": peer["agent_id"], "token": peer["token"]}
        try:
            self.native_summary({
                "action": "set", "group_id": group_id, "summary": "wrong scope overwrite",
                "expected_revision": second["revision"],
            }, agent=binding, expected_error="tool_summary_not_found")
        finally:
            self.public_call("collab", {
                "action": "leave", "client_id": self.client_id,
                "agent_id": peer["agent_id"], "token": peer["token"],
            })
        _, preserved = self.group(group_id)
        self.check(preserved["summary"] == SECOND and preserved["revision"] == second["revision"]
                   and preserved["call_count"] == second["call_count"]
                   and preserved["covered_call_count"] == second["covered_call_count"],
                   "stale revisions and a different authenticated agent cannot alter the owner group")

        ten_more = self.calls(10)
        self.check(not self.reminders(ten_more), "ten calls after an update remain below the next reminder threshold")
        eleven_more = self.calls(1)
        next_reminder_count = second["call_count"] + 11
        self.assert_reminder(eleven_more, second, next_reminder_count)
        _, final = self.group(group_id)
        self.check(final["call_count"] == next_reminder_count
                   and final["covered_call_count"] == second["covered_call_count"],
                   "only real work advances the group count across both reminder periods")
        renewed = self.update(final, SECOND)
        proactive = self.proactive_update(renewed)
        forged = self.response("return %s;" % json.dumps({
            "_meta": {"ssmcp.tool_summary": {
                "schema_version": 1, "group_id": "tool_group_" + "f" * 32,
                "revision": 99, "call_count": 99, "covered_call_count": 0,
                "threshold": 10, "tool": TOOL,
            }},
            "structuredContent": {"tool_summary_reminders": [{"group_id": "forged"}]},
        }))
        self.check(not self.reminders(forged),
                   "JavaScript return data cannot manufacture a host summary reminder")
        failed = self.failed_calls()
        failed_reminder_count = proactive["call_count"] + 11
        self.assert_reminder(failed, proactive, failed_reminder_count)
        _, after_failures = self.group(group_id)
        self.check(after_failures["call_count"] == failed_reminder_count,
                   "native errors remain in the same logical group instead of being reset by diagnostics")
        final = self.update(after_failures, "Checked file lookup results")
        self.mouse(front, final["summary"], 0)
        self.wait_group(front, group_id, lambda item: not item["expanded"], "collapse the updated tool group")
        self.frames_for(front, "final-folded", final["summary"], final["call_count"])
        self.check(self.history_messages() == messages_before,
                   "the summary lifecycle adds no user or assistant chat body")
        return {
            "status": "passed", "checks": len(self.checks), "passed_checks": self.checks,
            "frames": self.frames, "summary_receipts": self.receipts, "metrics": self.metrics,
            "final_group": {key: final[key] for key in (
                "group_id", "revision", "summary", "call_count", "covered_call_count")},
            "transport": "production account router, native tools, code_mode, WebSocket, and owned PTY",
            "upstream_provider": "synthetic GitHub fixture",
            "models_started": False, "existing_clients_reloaded": False,
            "existing_services_touched": False, "real_user_credentials_used": False,
            "source_of_images": "actual owned PTY cells decoded by the independent VT fixture",
        }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--decoder", required=True, type=Path)
    parser.add_argument("--fixture-binary", required=True, type=Path)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--out", required=True, type=Path)
    parser.add_argument("--font", type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        parser.error("--source-revision must be the complete frozen source SHA")
    binaries = {name: args.bin_dir.resolve() / name for name in ("ssmcp", "ssmcpd")}
    binaries.update(decoder=args.decoder.resolve(), fixture=args.fixture_binary.resolve())
    if any(not path.is_file() or not os.access(path, os.X_OK) for path in binaries.values()):
        parser.error("all frozen executables must exist; this fixture never builds missing inputs")
    if args.font is None:
        matched = subprocess.run(["fc-match", "-f", "%{file}", "DejaVu Sans Mono"],
                                 check=True, capture_output=True, text=True, timeout=5)
        args.font = Path(matched.stdout.strip())
    if not args.font.is_file():
        parser.error("pass an existing monospace TrueType/OpenType font with --font")
    args.out.mkdir(parents=True, exist_ok=True)
    if any(args.out.iterdir()):
        parser.error("use an empty output directory instead of overwriting previous evidence")
    before = {name: digest(path) for name, path in binaries.items()}
    os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(binaries["fixture"])

    def interrupted(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    started = time.monotonic()
    report = {"status": "failed", "source_revision": args.source_revision}
    with tempfile.TemporaryDirectory(prefix="ssmcp-tool-summary-ui-") as temporary:
        harness = ToolSummaries(Path(temporary), args.bin_dir.resolve(),
                                binaries["decoder"], args.out, args.font.resolve())
        try:
            report = harness.run()
        except Exception as error:
            detail = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    detail = detail.replace(secret, "[redacted]")
            report["error"] = detail
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
        after = {name: digest(path) for name, path in binaries.items()}
        if before != after:
            report.update(status="failed", error="a frozen executable changed during the test")
        sources = [
            "e2e_tool_summaries.py", "e2e_interactions.py", "e2e_input_history.py",
            "e2e_composer.py", "e2e_dashboard.py", "e2e_client_ui.py",
            "e2e_account_gateway.py", "e2e.py", "render_ui_snapshot.py",
        ]
        report.update(source_revision=args.source_revision,
                      elapsed_seconds=round(time.monotonic() - started, 3),
                      binaries_sha256_before=before, binaries_sha256_after=after,
                      source_sha256={name: digest(Path(__file__).with_name(name)) for name in sources},
                      font_sha256=digest(args.font), inputs_unchanged=before == after)
        encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        if any(secret and secret in encoded for secret in [harness.secret, *harness.sensitive]):
            raise AssertionError("the shareable report contains a private fixture secret")
        (args.out / "report.json").write_text(encoded, encoding="utf-8")
        print(json.dumps({"status": report["status"], "checks": report.get("checks", len(harness.checks)),
                          "frames": len(harness.frames), "error": report.get("error")}, ensure_ascii=False))
    if report["status"] != "passed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
