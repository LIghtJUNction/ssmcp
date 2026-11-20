#!/usr/bin/env python3
"""Check finish decisions through the real terminal keyboard and account router.

The producer uses only already-built, source-bound executables. It creates
disposable PTYs and an isolated fixture account; no model or installed account
is used. Its keyboard input simulates the local user in that fixture. A passing
fixture decision is never permission to finish a real user's conversation.

Run after the final gate has frozen matching client, router-test and VT decoder
executables, inside the same private-network acceptance sandbox as other E2Es:

  SSMCP_E2E_FIXTURE_BINARY=/frozen/ssmcp-tests \
    python3 -B scripts/e2e_human_finish.py --bin-dir /frozen \
      --decoder /frozen/vt_snapshot --source-revision FULL_FINAL_SHA \
      --out /owned/human-finish

The outer controller must verify its passed gate, exact source manifest and
these input hashes. This producer records its own input hashes before/after;
it does not manufacture a build receipt or replace that outer verification.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import sys
import tempfile
import time

sys.dont_write_bytecode = True

from e2e_composer import DOWN, ENTER, HOME
from e2e_dashboard import sha256, text
from e2e_input_history import InputHistory


SHIFT_UP = b"\x1b[1;2A"
CSI_RIGHT, CSI_LEFT = b"\x1b[C", b"\x1b[D"
SS3_RIGHT, SS3_LEFT = b"\x1bOC", b"\x1bOD"
SOURCE_REVISION = re.compile(r"[0-9a-f]{40}\Z")


def input_hashes(binary_dir, decoder, fixture_binary):
    scripts = Path(__file__).resolve().parent
    paths = {"ssmcp": binary_dir / "ssmcp", "ssmcpd": binary_dir / "ssmcpd",
             "vt_snapshot": decoder, "router_test": fixture_binary}
    # Reused helpers are part of the reviewed source. Recording all E2E helper
    # files also includes their transitive imports without an executable scan.
    paths.update({"source/" + path.name: path for path in scripts.glob("e2e*.py")})
    return {name: {"path": str(path.resolve(strict=True)), "sha256": sha256(path)}
            for name, path in sorted(paths.items())}


class HumanFinish(InputHistory):
    def __init__(self, directory, binary_dir, decoder, output):
        super().__init__(directory, binary_dir, decoder, output)
        self.fixture_timeout_seconds = 180
        self.frame_records = []
        self.case_number = 0

    def native(self, name, arguments, member):
        result = self.response(
            "return await call(%s, %s, %s);" % (
                json.dumps(self.client_id), json.dumps(name), json.dumps(arguments)),
            agent={"client_id": self.client_id, "id": member["agent_id"],
                   "token": member["token"]},
        )
        self.check(not self.messages(result),
                   "a fixture observation does not consume an unrelated user requirement")
        native = result["structuredContent"]["result"]
        if native.get("isError") is True:
            raise AssertionError("the authenticated native fixture operation was rejected")
        return native["structuredContent"]

    def member(self, label):
        member = self.public_call("collab", {
            "action": "join", "client_id": self.client_id,
            "name": "Human finish fixture " + label,
            "role": "Registered test participant; no model process",
        })
        self.sensitive.append(member["token"])
        return member

    def finish(self, member, reason, *, request_id=None, key=None):
        arguments = {
            "client_id": self.client_id, "reason": reason, "timeout_ms": 2000,
            "agent": {"id": member["agent_id"], "token": member["token"]},
        }
        if request_id is not None:
            arguments["request_id"] = request_id
        if key is not None:
            arguments["idempotency_key"] = key
        result = self.public_call("request_finish", arguments)
        self.check(not result.get("user_messages"),
                   "a finish observation does not consume an unrelated queued requirement")
        return result

    def question(self, member, request_id):
        return self.native("ssmcp.ui.questions", {"id": request_id}, member)["question"]

    def pending(self, member, result, request_id=None):
        actual_id = result["request_id"]
        self.check(isinstance(actual_id, str) and bool(actual_id)
                   and (request_id is None or actual_id == request_id),
                   "the pending finish response retains its exact server-issued request ID")
        self.check(result.get("timed_out") is True
                   and result.get("default_decision") == "no"
                   and result.get("decision") is None and result.get("approved") is False
                   and result["request"]["finish"]["status"] == "pending"
                   and result["request"]["finish"]["approved"] is False
                   and result["request"]["finish"]["decision"] is None
                   and result["request"]["requester"]["thread_id"] == member["agent_id"],
                   "timeout leaves the exact requesting thread pending and never invents human consent")
        return actual_id

    def current_history(self):
        sessions = self.observe("ssmcp.ui.history", {"action": "list"})["sessions"]
        selected = [entry["id"] for entry in sessions if entry["current"]]
        if len(selected) != 1:
            raise AssertionError("expected one real selected conversation")
        return selected[0]

    def history_path(self, history_id):
        if not re.fullmatch(r"conversation_[0-9a-f]{32}", history_id):
            raise AssertionError("the fixture received an invalid history ID")
        return self.state_home / "ssmcp" / "conversations" / (history_id + ".jsonl")

    def history_answers(self, history_id, marker):
        rows = [json.loads(line) for line in self.history_path(history_id).read_bytes().splitlines()]
        return [row for row in rows if row.get("type") == "message"
                and row.get("kind") == "answer" and marker in row.get("content", "")]

    def activity(self, front):
        # The column list comes from the live UI. It is not an old fixed
        # "agents" label or an assumed eight-column index.
        self.activate_column(front, "conversation")
        self.focus_input(front)
        self.wait_state(front, lambda state: state["view"] == "activity",
                        "the real conversation panel did not regain input focus")

    def clear_own_draft(self, front):
        self.activity(front)
        if self.ui("/display/conversation")["draft"]["text"]:
            front.send(b"\x03")
            self.wait_state(front, lambda state: not state["draft"]["text"],
                            "the fixture's own unsent draft did not clear")

    def resume(self, front, history_id):
        self.clear_own_draft(front)
        front.send("/resume\r")
        self.wait_state(front, lambda state: state["view"] == "history",
                        "the real /resume command did not open saved conversations")
        sessions = self.observe("ssmcp.ui.history", {"action": "list"})["sessions"]
        matches = [index for index, entry in enumerate(sessions) if entry["id"] == history_id]
        if len(matches) != 1:
            raise AssertionError("the exact saved fixture conversation is not selectable")
        front.send(HOME + DOWN * matches[0] + ENTER)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            if self.current_history() == history_id:
                self.activity(front)
                return
        raise AssertionError("the public terminal /resume route selected the wrong conversation")

    def seed_history(self, front, marker):
        record = self.submit_source(front, marker)
        self.expect_delivery(self.work(), record, agent_id=None)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            if self.record(self.queue(), message_id=record["id"])["status"] == "delivered":
                return self.current_history()
        raise AssertionError("the seed input was not delivered before history navigation")

    def capture(self, front, name, marker):
        shot = front.until(lambda value: marker in text(value),
                           "the expected real terminal evidence", cells=True)
        rows, columns = shot["size"]
        if rows != 30 or columns not in (120, 48):
            raise AssertionError("the terminal evidence has unexpected dimensions")
        encoded = json.dumps(shot, ensure_ascii=False, indent=2) + "\n"
        self.check(not any(secret and secret in encoded for secret in [self.secret, *self.sensitive]),
                   name + ": the terminal snapshot contains no fixture credential")
        path = self.output / (name + ".json")
        path.write_text(encoded, encoding="utf-8")
        self.frame_records.append({
            "path": path.name, "sha256": sha256(path), "rows": rows, "cols": columns,
            "source_kind": "owned_pty_capture", "keyboard_actor": "fixture_simulated_user",
        })

    def open_question(self, front, request_id, marker):
        front.send(SHIFT_UP)
        self.wait_state(front, lambda state: state["view"] == "questions"
                        and state["answering"] == request_id,
                        "Shift+Up did not reach the real question keyboard route")
        front.until(lambda shot: marker in text(shot),
                    "Shift+Up did not open the exact pending fixture question")

    def decision_case(self, front, choice, columns, right, left, *, other_history=None):
        self.case_number += 1
        label = f"{choice}-{columns}-{self.case_number}"
        marker = "FINISH_" + label.upper().replace("-", "_")
        self.clear_own_draft(front)
        original_history = self.current_history()
        member = self.member(label)
        reason = marker + "\nThe fixture is ready for an explicit local decision."
        pending = self.finish(member, reason, key="human_finish_" + label.replace("-", "_"))
        request_id = self.pending(member, pending)
        if other_history is not None:
            self.resume(front, other_history)
        visible_history = self.current_history()
        unrelated_before = (self.history_path(visible_history).read_bytes()
                            if visible_history != original_history else None)
        self.pending(member, self.finish(member, reason, request_id=request_id), request_id)

        self.activity(front)
        front.resize(30, columns)
        draft = "PARKED_FINISH_DRAFT 界"
        front.send(draft.encode("utf-8") + CSI_LEFT)
        original = self.wait_state(front, lambda state: state["draft"]["text"] == draft
                                   and state["draft"]["cursor"] == len(draft.encode("utf-8")) - len("界".encode("utf-8")),
                                   "the parked Unicode draft did not enter the real composer")
        composition = {key: original[key] for key in ("draft", "target", "editing")}
        self.open_question(front, request_id, marker)
        front.until(lambda shot: "● No" in text(shot), "No is the initial local finish choice")
        comment = "Yes is only comment: DECISION_FOR_" + marker
        front.send(b"\t" + comment.encode("utf-8") + b"\x1b[Z")
        self.wait_state(front, lambda state: state["view"] == "questions"
                        and state["answering"] == request_id and state["focus"] == "content",
                        "Tab/comment/Shift+Tab did not return to the finish choice")
        front.until(lambda shot: "● No" in text(shot),
                    "writing the optional comment does not change the default No")

        # These bytes enter the OS PTY and traverse input decoding, chat_key,
        # and question_key. No question mutation API chooses a human answer.
        front.send(right)
        front.until(lambda shot: "● Yes" in text(shot),
                    "Right selected Yes without leaving the Questions panel")
        self.check(self.ui("/display/conversation")["view"] == "questions",
                   "the public Right key route remains on Questions")
        before_enter = self.question(member, request_id)
        self.check(before_enter["finish"]["status"] == "pending"
                   and before_enter["finish"]["decision"] is None
                   and before_enter["finish"]["approved"] is False,
                   "selecting Yes without Enter never approves a finish request")
        if choice == "no":
            front.send(left)
            front.until(lambda shot: "● No" in text(shot),
                        "Left selected No without leaving the Questions panel")
            self.check(self.ui("/display/conversation")["view"] == "questions",
                       "the public Left key route remains on Questions")
        front.send(ENTER)
        result = self.finish(member, reason, request_id=request_id)
        finish = result["request"]["finish"]
        self.check(result["request_id"] == request_id
                   and result["decision"] == finish["decision"]
                   and finish["decision"]["actor"]["origin"] == "human"
                   and finish["decision"]["choice"] == choice
                   and finish["decision"]["comment"] == comment
                   and result["approved"] is (choice == "yes")
                   and finish["approved"] is (choice == "yes")
                   and finish["status"] == ("approved" if choice == "yes" else "denied"),
                   "only the terminal Enter commits the selected explicit fixture human decision")
        after = self.ui("/display/conversation")
        self.check({key: after[key] for key in composition} == composition,
                   "the decision preserves the main draft, UTF-8 cursor and recipient")
        self.check(self.current_history() == visible_history,
                   "recording a decision does not change the selected conversation")
        self.capture(front, "human-finish-" + label,
                     "Yes by You" if choice == "yes" else "No by You")
        if unrelated_before is not None:
            self.check(self.history_path(visible_history).read_bytes() == unrelated_before,
                       "an answer to conversation A leaves selected conversation B byte-for-byte unchanged")
            self.check(len(self.history_answers(original_history, comment)) == 1,
                       "the switched-history finish decision is recorded once in its original conversation")
            self.activity(front)
            shot = front.snapshot(cells=True)
            self.check(marker not in text(shot) and comment not in text(shot),
                       "conversation B's activity projection does not acquire A's finish reply")
        self.clear_own_draft(front)

    def ordinary_cross_history(self, front, original_history, other_history):
        self.resume(front, original_history)
        member = self.member("ordinary-history")
        marker = "HISTORY_A_QUESTION"
        question = self.native("ssmcp.ui.ask", {
            "question": marker + "\nReply only to the original conversation.",
            "idempotency_key": "human_finish_history_question",
        }, member)
        request_id = question["id"]
        self.resume(front, other_history)
        unrelated_before = self.history_path(other_history).read_bytes()
        front.resize(30, 48)
        self.open_question(front, request_id, marker)
        answer = "ANSWER_FOR_A_ONLY 界"
        front.send(answer.encode("utf-8") + ENTER)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            recorded = self.question(member, request_id)
            if recorded["state"] == "answered":
                break
        else:
            raise AssertionError("the keyboard answer was not retained")
        self.check(recorded["answer"] == answer and recorded["answer_actor"]["origin"] == "human",
                   "the ordinary answer comes from the real terminal keyboard route")
        self.check(len(self.history_answers(original_history, answer)) == 1
                   and self.history_path(other_history).read_bytes() == unrelated_before
                   and self.current_history() == other_history,
                   "an ordinary A question answered in B writes once to A and leaves B selected and unchanged")
        repeated = self.native("ssmcp.ui.ask", {
            "question": marker + "\nReply only to the original conversation.",
            "idempotency_key": "human_finish_history_question",
        }, member)
        self.check(repeated["id"] == request_id and repeated["created"] is False
                   and len(self.history_answers(original_history, answer)) == 1
                   and self.history_path(other_history).read_bytes() == unrelated_before,
                   "retrying the original question after the switch keeps its ID and captured history owner")
        self.capture(front, "human-answer-original-history-48", answer)
        self.activity(front)
        self.check(answer not in text(front.snapshot()),
                   "B's conversation activity does not display A's ordinary answer")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "-q"],
                       cwd=self.workspace, env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5)
        # Create both histories using real user input in separate owned clients,
        # not by inventing JSONL or changing the production history singleton.
        first = self.start_front("human-finish-history-b")
        other_history = self.seed_history(first, "HISTORY_B_SEED")
        first.disconnect()
        self.check(first.process.returncode == 0, "the history-B fixture client exits cleanly")
        first.close()
        front = self.start_front("human-finish-history-a")
        original_history = self.seed_history(front, "HISTORY_A_SEED")
        self.check(original_history != other_history, "the two real fixture clients create distinct histories")
        self.decision_case(front, "no", 120, CSI_RIGHT, CSI_LEFT)
        self.decision_case(front, "no", 48, SS3_RIGHT, SS3_LEFT)
        self.ordinary_cross_history(front, original_history, other_history)
        self.resume(front, original_history)
        self.decision_case(front, "yes", 120, CSI_RIGHT, CSI_LEFT)
        self.decision_case(front, "yes", 48, SS3_RIGHT, SS3_LEFT, other_history=other_history)
        self.check(not self.queue()["queue"] or all(item["status"] == "delivered"
                   for item in self.queue()["queue"]), "the fixture leaves no new user requirement pending")
        final, restored = front.disconnect()
        self.check(not final["alternate_screen"] and restored == front.original_termios
                   and front.process.returncode == 0,
                   "the owned decision terminal exits cleanly and restores its termios")
        self.metrics["pty"] = {"bytes": len(front.transcript),
                               "sha256": hashlib.sha256(front.transcript).hexdigest()}
        front.close()


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("human-finish fixture interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if not SOURCE_REVISION.fullmatch(args.source_revision):
        parser.error("source-revision must be the exact full final commit SHA")
    fixture = os.environ.get("SSMCP_E2E_FIXTURE_BINARY")
    if not fixture:
        parser.error("SSMCP_E2E_FIXTURE_BINARY must identify the final source-bound library test executable")
    binary_dir = args.bin_dir.resolve(strict=True)
    decoder = args.decoder.resolve(strict=True)
    fixture_binary = Path(fixture).resolve(strict=True)
    for executable in (binary_dir / "ssmcp", binary_dir / "ssmcpd", decoder, fixture_binary):
        if not executable.is_file() or not os.access(executable, os.X_OK):
            parser.error("missing executable: " + str(executable))
    output = args.out.absolute()
    if output.exists() or output.is_symlink():
        parser.error("out must be a new owned directory; previous evidence is never reused")
    output.mkdir(parents=True, mode=0o700)
    started = time.monotonic()
    before = input_hashes(binary_dir, decoder, fixture_binary)
    report = {"schema": "ssmcp.acceptance.human_finish.v1", "status": "running",
              "source_revision": args.source_revision, "source_kind": "owned_pty_capture",
              "keyboard_actor": "fixture_simulated_user", "real_user_finish_permission": False,
              "transport": "private account router + OAuth + WebSocket + owned OS PTY + independent VT decoder",
              "binaries": {name: before[name]["sha256"] for name in
                           ("ssmcp", "ssmcpd", "vt_snapshot", "router_test")},
              "sources": {name: value["sha256"] for name, value in before.items()
                          if name.startswith("source/")},
              "inputs_before": before, "checks": [], "frames": []}
    (output / "report.json").write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    harness = None
    failed = None
    with tempfile.TemporaryDirectory(prefix="work-", dir=output) as directory:
        try:
            harness = HumanFinish(Path(directory), binary_dir, decoder, output)
            harness.run()
            report["status"] = "passed"
        except BaseException as error:
            failed = error
            report["status"] = "failed"
            message = str(error)
            if harness is not None:
                for secret in [harness.secret, *harness.sensitive]:
                    if secret:
                        message = message.replace(secret, "[redacted]")
            report["error"] = re.sub(r"\b[0-9a-fA-F]{64}\b", "[redacted-token]", message)[:12000]
        finally:
            if harness is not None:
                cleanup_errors = []
                for cleanup in (harness.close_frontends, harness.cleanup):
                    try:
                        cleanup()
                    except BaseException as error:
                        cleanup_errors.append(type(error).__name__)
                if cleanup_errors:
                    report["status"] = "failed"
                    report["cleanup_errors"] = cleanup_errors
                report["checks"] = harness.checks
                report["metrics"] = harness.metrics
                report["frames"] = harness.frame_records
            try:
                report["inputs_after"] = input_hashes(binary_dir, decoder, fixture_binary)
            except BaseException as error:
                report["inputs_after"] = {}
                report["input_error_type"] = type(error).__name__
            if report["inputs_after"] != before:
                report["status"] = "failed"
                report["input_error"] = "source helpers or executables changed during acceptance"
            report["duration_seconds"] = round(time.monotonic() - started, 3)
            (output / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n",
                                                encoding="utf-8")
    if failed is not None or report["status"] != "passed":
        print("FAIL human finish; see the owned report", file=sys.stderr)
        return 1
    print(f"PASS human finish: {len(report['checks'])} checks, {len(report['frames'])} real PTY captures")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
