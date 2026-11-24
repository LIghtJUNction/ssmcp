#!/usr/bin/env python3
"""Accept native TUI right-side inputs and editable quotes through an owned PTY.

Explicit generated origins exercise the canonical publisher/render/reply path.
Real scheduler production is covered separately by e2e_durable_wait.py.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time
import traceback
import uuid

from e2e_account_relay import wait_for
from e2e_client_ui import empty_composer_ready
from e2e_composer import Composer, ENTER
from e2e_dashboard import text, region, sha256


class Replies(Composer):
    def __init__(self, directory, options, report):
        self.options, self.report = options, report
        self.fixture_binary = options.fixture_binary
        super().__init__(directory, options.bin_dir, options.bin_dir / "examples/vt_snapshot")

    def frame(self, front, label):
        shot = front.snapshot(cells=True)
        path = self.options.report.parent / (label + ".cells.json")
        path.write_text(json.dumps(shot, ensure_ascii=False) + "\n")
        (self.options.report.parent / (label + ".txt")).write_text(text(shot) + "\n")
        return {"path": str(path), "sha256": sha256(path)}, shot

    def accepted(self, front, expected, previous):
        def find():
            front.pump()
            rows = [row for row in self.queue()["queue"]
                    if row["id"] not in previous and row.get("text") == expected
                    and row.get("target") == self.member["agent_id"]]
            if not rows:
                return None
            if len(rows) != 1:
                raise AssertionError("one physical submission produced multiple matching queue records")
            return rows[0] if rows[0].get("status") == "queued" else None
        return wait_for(find, "the original physical quote submission in its exact native queue")

    def delivered(self, front, record, expected):
        response = self.work(member=self.member)
        self.expect_delivery(response, record, request_text=expected, agent_id=self.member["agent_id"])
        def final():
            front.pump()
            current = self.record(self.queue(), message_id=record["id"])
            return current if current["status"] == "delivered" else None
        final_record = wait_for(final, "the original quoted request's verified delivered state")
        self.check(not self.messages(self.work(member=self.member)),
                   "another bound work response does not duplicate the original quoted delivery")
        return {"record": final_record, "delivery": self.messages(response)[0]}

    def marker(self, front, marker):
        pane = self.ui("/display/panes/activity")
        found = []
        def locate(shot):
            found.clear()
            for y in range(pane["row"], pane["row"] + pane["rows"]):
                for x in range(pane["col"], pane["col"] + pane["cols"] - len(marker) + 1):
                    if all(shot["cells"][y][x + offset]["text"] == character
                           and not shot["cells"][y][x + offset]["continuation"]
                           for offset, character in enumerate(marker)):
                        found.append((x, y))
            return len(found) == 1
        shot = front.until(locate, "one original source marker in the actual activity pane", cells=True)
        x, y = found[0]
        self.check(x >= pane["col"] + pane["cols"] // 2,
                   "the original short input marker is physically aligned on the right side")
        return (x, y), pane, shot

    def select(self, front, marker):
        (x, y), pane, shot = self.marker(front, marker)
        sgr = f"\x1b[<0;{x + 1};{y + 1}"
        front.send((sgr + "M" + sgr + "m").encode())
        front.until(lambda shot: "[R 引用]" in text(shot)
                    and self.ui("/display/conversation")["focus"] == "content",
                    "physical source selection exposing the quote control")
        return {"x": x, "y": y, "pane": pane}

    def activate(self, front, activation):
        if activation == "R":
            front.send(b"R")
        else:
            pane = self.ui("/display/panes/input")
            x, y = pane["col"] + pane["cols"] - 6, pane["row"] - 1
            shot = front.snapshot(cells=True)
            self.check("[R 引用]" in shot["rows"][y], "the actual input header exposes the quote button")
            sgr = f"\x1b[<0;{x + 1};{y + 1}"
            front.send((sgr + "M" + sgr + "m").encode())
        front.until(lambda shot: self.ui("/display/conversation")["focus"] == "input",
                    "quoting returns focus to the real composer")

    def draft(self, front, value):
        self.focus_input(front)
        self.check(self.ui("/display/conversation")["draft"]["text"] == "",
                   "the owned draft is empty before seeding a new reply")
        front.send(value.encode())
        front.until(lambda shot: self.ui("/display/conversation")["draft"]["text"] == value,
                    "the full exact pre-quote draft")

    def quote_case(self, front, origin, source, message_id, activation, label=None):
        marker = source.splitlines()[0]
        before_ids = {record["id"] for record in self.queue()["queue"]}
        original_draft = "DRAFT_" + origin + " keep 中文"
        self.draft(front, original_draft)
        coordinate = self.select(front, marker)
        before_frame, shot = self.frame(front, origin + "-selected")
        if label:
            pane = coordinate["pane"]
            self.check(label in region(shot, pane["row"], pane["row"] + pane["rows"],
                                       pane["col"], pane["col"] + pane["cols"]),
                       "the physical Input preserves its generated provenance label: " + origin)
        expected = "> 回复消息 " + message_id + "\n> " + source.replace("\n", "\n> ") + "\n\n" + original_draft
        self.activate(front, activation)
        observed = self.ui("/display/conversation")
        self.check(observed["draft"]["text"] == expected
                   and observed["target"] == self.member["agent_id"]
                   and observed["native_recipient"] is None and observed["view"] == "activity",
                   "actual quote activation preserves the full source, original ID, draft and recipient")
        self.check({row["id"] for row in self.queue()["queue"]} == before_ids,
                   "quote activation never submits or duplicates a queued requirement")
        activations = [activation]
        if origin == "maintenance":
            self.check(bool(observed["draft"]["text"]), "only the known nonempty owned draft is cleared")
            front.send(b"\x03")
            front.until(lambda shot: self.ui("/display/conversation")["draft"]["text"] == "",
                        "clearing the owned nonempty quote draft")
            self.check({row["id"] for row in self.queue()["queue"]} == before_ids,
                       "clearing an owned quote draft does not alter the queue")
            self.draft(front, original_draft)
            self.select(front, marker)
            self.activate(front, "button")
            self.check(self.ui("/display/conversation")["draft"]["text"] == expected,
                       "R and the actual button quote the same original message identically")
            activations.append("button")
        quote_frame, _ = self.frame(front, origin + "-quoted")
        front.send(ENTER)
        record = self.accepted(front, expected, before_ids)
        self.check(record["text"] == expected and "<ssmcp-references-v1>" not in record["text"],
                   "quoted @/$ and unmatched quoted punctuation remain ordinary accepted source text")
        delivered = self.delivered(front, record, expected)
        self.check(self.ui("/display/conversation")["draft"]["text"] == "",
                   "verified ordinary submission clears the sent quote draft")
        return {"origin": origin, "source": source, "original_message_id": message_id,
                "original_draft": original_draft, "quoted_draft": expected, "activations": activations,
                "coordinate": coordinate, "selected_frame": before_frame, "quoted_frame": quote_frame,
                **delivered}

    def run_replies(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "-q"], cwd=self.workspace,
                       env={**os.environ, **self.safe_git_env}, check=True, timeout=5)
        front = self.frontend("reply-acceptance", self.config("reply-acceptance"))
        self.oauth_login(front.credentials)
        front.until(empty_composer_ready, "the actual owned chat terminal")
        native_tools = {tool["name"] for tool in self.native_catalog(self.client_id)}
        self.check({"ssmcp.ui.message", "ssmcp.ui.get", "ssmcp.ui.queue"} <= native_tools,
                   "the original native session advertises the exercised Input and UI contracts")
        self.member = self.public_call("collab", {"action": "join", "client_id": self.client_id,
            "name": "Owned reply recipient", "role": "worker"})
        self.sensitive.append(self.member["token"])
        wait_for(lambda: any(agent["agent_id"] == self.member["agent_id"] for agent in self.queue()["agents"]),
                 "the original exact fixture recipient")
        self.recipient(front, self.member)
        self.activate_column(front, "conversation")
        state = self.ui("/display/conversation")
        self.check(state["target"] == self.member["agent_id"] and state["editing"] is None,
                   "quote acceptance uses the existing exact recipient without a held queue edit")
        original = "Q_HUMAN_" + uuid.uuid4().hex[:10]
        before = {row["id"] for row in self.queue()["queue"]}
        self.draft(front, original)
        front.send(ENTER)
        human_record = self.accepted(front, original, before)
        human_delivery = self.delivered(front, human_record, original)
        stable_id = "message_" + hashlib.sha256(
            b"ssmcp.history.request.v1" + bytes([0]) + human_record["id"].encode()
            + bytes([0]) + original.encode()).hexdigest()[:32]
        self.report["original_human"] = {"source": original, "message_id": stable_id, **human_delivery}
        cases = [self.quote_case(front, "human", original, stable_id, "R")]
        for origin, activation, label in [
            ("system_hook", "button", "系统提示"), ("agent_timer", "R", "定时提示"),
            ("worker_event", "button", "事件提示"), ("maintenance", "R", "维护提示"),
        ]:
            marker = "Q_" + origin.upper() + "_" + uuid.uuid4().hex[:6]
            source = marker + '\n@"unterminated file\n$quote_skill\n' + chr(96) + "unclosed"
            before = [(item["id"], item["status"]) for item in self.queue()["queue"]]
            args = {"kind": "input", "input_origin": origin, "idempotency_key": "quote_" + uuid.uuid4().hex,
                    "text": source, "finish_task": False}
            code = "await collab.bind(%s,%s); return await call(%s,'ssmcp.ui.message',%s);" % (
                json.dumps(self.client_id), json.dumps({"agent_id": self.member["agent_id"],
                                                      "token": self.member["token"]}),
                json.dumps(self.client_id), json.dumps(args))
            outer = self.response(code)
            native = outer["structuredContent"]["result"]
            receipt = native.get("structuredContent", {})
            self.check(not native.get("isError") and not self.messages(outer)
                       and receipt.get("accepted") is True and receipt.get("recorded") is True
                       and receipt.get("duplicate") is False and receipt.get("kind") == "input"
                       and receipt.get("input_origin") == origin
                       and receipt.get("completion_permission") is False and bool(receipt.get("message_id")),
                       "one canonical generated Input publication has its actual passive native receipt")
            self.check([(item["id"], item["status"]) for item in self.queue()["queue"]] == before,
                       "generated Input publication does not create or consume a human requirement")
            case = self.quote_case(front, origin, source, receipt["message_id"], activation, label)
            case["publication_receipt"] = receipt
            cases.append(case)
        self.report["cases"] = cases
        self.check(len(self.gateway_deliveries) == 6,
                   "the original human source and five physical quote submissions have six exact deliveries")
        self.check(self.external_invocations("reply-acceptance") == [],
                   "the actual PTY reply acceptance starts no external agent")
        _, restored = front.disconnect()
        self.check(restored == front.original_termios and front.process.returncode == 0,
                   "normal operator disconnect restores the owned terminal and exits successfully")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    parser.add_argument("--build-source-commit", required=True)
    parser.add_argument("--report", type=Path, required=True)
    options = parser.parse_args()
    options.bin_dir = options.bin_dir.resolve(strict=True)
    options.fixture_binary = options.fixture_binary.resolve(strict=True)
    options.report = options.report.resolve()
    options.report.parent.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {"status": "failed", "scope": "Actual owned POSIX PTY, independent VT decoder, native Input publisher and OAuth HTTP delivery",
              "build_source_commit": options.build_source_commit, "models_started": False,
              "generated_origin_producer_scope": "canonical publisher; timer scheduler tested separately",
              "binaries": {name: {"path": str(path), "sha256": sha256(path)} for name, path in
                  [("client", options.bin_dir / "ssmcp"), ("fixture", options.fixture_binary),
                   ("decoder", options.bin_dir / "examples/vt_snapshot")]}}
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-input-replies-") as directory:
        harness = Replies(Path(directory), options, report)
        try:
            harness.run_replies()
            report["status"] = "passed"
        except Exception as error:
            report["error"] = f"{type(error).__name__}: {error}"
            report["traceback"] = traceback.format_exc()
            if harness.last_frontend is not None:
                try:
                    harness.frame(harness.last_frontend, "failure")
                except Exception:
                    pass
            harness.diagnostics()
        finally:
            for front in harness.frontends:
                front.close()
            try:
                harness.cleanup()
            except Exception as error:
                report["status"] = "failed"
                report["cleanup_error"] = str(error)
            report["checks"] = harness.checks
            report["cleanup"] = {"fixture_exit_code": harness.account_gateway.exit_code,
                "owned_processes": [{"pid": process.pid, "exit_code": process.poll(),
                                     "reaped": process.returncode is not None} for process in harness.processes]}
    report["seconds"] = round(time.monotonic() - started, 3)
    options.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "report": str(options.report),
                     "checks": len(report["checks"]), "error": report.get("error")}, ensure_ascii=False))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
