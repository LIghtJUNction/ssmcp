#!/usr/bin/env python3
"""Verify nonblocking user questions and Markdown messages in an owned PTY.

Build ssmcp, ssmcpd, and examples/vt_snapshot, then run:
  python3 scripts/e2e_questions.py --bin-dir target/debug --report /tmp/questions.json

Reuses the isolated SSH/OAuth fixture from e2e_composer. All processes, ports,
keys, agents, and workspace files are owned by this test. No model is started.
Optional --out writes actual VT cell snapshots without connection credentials.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import signal
import subprocess
import tempfile
import time

from e2e_account_relay import wait_for
from e2e_composer import Composer, DELETE, ENTER, ESC, LEFT
from e2e_client_ui import column, empty_composer_ready
from e2e_dashboard import sha256, text


SHIFT_UP = b"\x1b[1;2A"


class Questions(Composer):
    def __init__(self, directory, binary_dir, decoder, output=None):
        super().__init__(directory, binary_dir, decoder)
        self.output = output

    def save_shot(self, name, front):
        if self.output is not None:
            self.output.mkdir(parents=True, exist_ok=True)
            shot = front.snapshot(cells=True)
            (self.output / f"questions-{name}.json").write_text(
                json.dumps(shot, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
            )

    def ask(self, question, **options):
        return self.observe("ssmcp.ui.ask", {"question": question, **options})

    def question(self, question_id):
        return self.observe("ssmcp.ui.questions", {"id": question_id})["question"]

    def wait_question(self, front, question_id, expected):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            question = self.question(question_id)
            if question["state"] == expected:
                return question
            if front.process.poll() is not None:
                break
        raise AssertionError(f"question did not reach {expected}")

    def wait_conversation(self, front, predicate, description):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            state = self.ui("/display/conversation")
            if predicate(state):
                return state
            if front.process.poll() is not None:
                break
        raise AssertionError(description)

    @staticmethod
    def composition(state):
        return {name: state[name] for name in ("draft", "target", "editing")}

    def activity(self, front):
        # Escape is decoded after a short timeout. Each transition gets a
        # separate input write, so it cannot accidentally become an Alt key.
        for _ in range(3):
            state = self.ui("/display/conversation")
            if state["view"] == "activity":
                front.until(lambda shot: column(shot) == ("conversation", 2, 8)
                            and not shot["hide_cursor"],
                            "the restored conversation column and ordinary composer")
                return state
            front.send(ESC)
            front.pump(0.15)
        raise AssertionError("Escape did not return from questions to activity")

    def details(self, front, marker):
        front.send(SHIFT_UP)
        front.until(
            lambda shot: column(shot) == ("questions", 5, 8)
            and marker in text(shot) and not shot["hide_cursor"],
            "Shift+Up opening the expected question with its answer input",
        )

    def native_rejected(self, name, arguments, expected_error):
        # A native validation failure may reject call() before producing an MCP
        # envelope. Catch only to inspect it; unrelated transport failures must
        # still fail the exact expected-message assertion below.
        response = self.response(
            "try { const native = await call(%s, %s, %s); "
            "return {rejected: native.isError === true, "
            "error: (native.content || []).map(item => item.text || '').join(' ')}; "
            "} catch (error) { return {rejected: true, error: String(error)}; }" % (
                json.dumps(self.client_id), json.dumps(name), json.dumps(arguments)
            )
        )
        rejection = response["structuredContent"]["result"]
        self.check(rejection.get("rejected") is True
                   and expected_error in rejection.get("error", ""),
                   f"{name} rejects unsupported message input with the expected validation error")
        self.check(not self.messages(response), "a rejected UI call does not consume user requirements")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(
            ["git", "-c", "init.defaultBranch=main", "init", "-q"],
            cwd=self.workspace, env={**os.environ, **self.safe_git_env},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5,
        )
        front = self.frontend("questions", self.config("questions"))
        self.oauth_login(front.credentials)
        front.until(
            empty_composer_ready,
            "the live chat composer in the isolated terminal",
        )
        member = self.public_call("collab", {
            "action": "join", "client_id": self.client_id,
            "name": "Question fixture agent", "role": "question input test",
        })
        self.sensitive.append(member["token"])

        # Choose an exact recipient before drafting. The registered member is
        # fixture data, not a spawned model or an external coding harness.
        self.agents(front)
        front.until(lambda shot: "Question fixture agent" in text(shot),
                    "the actual fixture collaboration member")
        # Sessions also displays native recipients, which can arrive before
        # the independent local steering roster. A name on screen does not
        # prove that the exact local recipient is ready for selection.
        def matching_member():
            matches = [item for item in self.queue()["agents"]
                       if item["agent_id"] == member["agent_id"]]
            return matches[0] if len(matches) == 1 else None

        wait_for(matching_member, "the exact question member in the local steering roster")
        self.highlight_recipient(front, member)
        front.send(ENTER)
        front.until(lambda shot: column(shot) == ("conversation", 2, 8)
                    and "To Question fixture agent" in text(shot),
                    "Enter selecting an exact member recipient")
        selected = self.ui("/display/conversation")
        self.check(selected["target"] == member["agent_id"]
                   and selected["native_recipient"] is None,
                   "question setup selects the exact local member rather than a native namesake")
        ordinary = "QUESTIONS_ORDINARY_DRAFT 保留文字界"
        front.send(ordinary.encode("utf-8") + LEFT)
        original = self.wait_conversation(
            front, lambda state: state["draft"]["text"] == ordinary,
            "the original Unicode draft was not accepted",
        )
        original = self.composition(original)
        self.check(original["target"] == member["agent_id"]
                   and original["draft"]["cursor"] == len(ordinary.encode()) - len("界".encode()),
                   "the parked draft has an exact recipient and a UTF-8 cursor before its final character")

        started = time.monotonic()
        first = self.ask("Which output format?\n\nDETAIL_OPTIONS_FIRST",
                         title="Output format", options=["Markdown", "HTML"],
                         idempotency_key="question_fixture_first")
        second = self.ask("What should happen next?\n\nDETAIL_FREE_SECOND",
                          title="Next step")
        third = self.ask("This question can be skipped.\n\nDETAIL_SKIP_THIRD",
                         title="Optional question")
        self.metrics["ask_seconds"] = round(time.monotonic() - started, 3)
        self.check(all(item["blocking"] is False and item["state"] == "pending"
                       for item in (first, second, third)),
                   "each ask call returns pending immediately without awaiting a user answer")
        self.check(len({item["id"] for item in (first, second, third)}) == 3,
                   "multiple pending questions retain distinct identities")
        pending = self.observe("ssmcp.ui.questions", {"state": "pending"})
        self.check(pending["pending_count"] == 3 and pending["total"] == 3,
                   "later tool calls complete while earlier questions remain unanswered")
        again = self.ask("Which output format?\n\nDETAIL_OPTIONS_FIRST",
                         title="Output format", options=["Markdown", "HTML"],
                         idempotency_key="question_fixture_first")
        # Authenticated ask receipts omit the global count. Inspect only this
        # caller's visible pending questions after retrying the same request.
        after_retry = self.observe("ssmcp.ui.questions", {"state": "pending"})
        self.check(again["id"] == first["id"] and again["created"] is False
                   and after_retry["pending_count"] == 3,
                   "retrying an idempotent ask does not add another queued question")
        self.check(self.composition(self.ui("/display/conversation")) == original,
                   "background questions do not steal or replace the normal draft")

        shown = self.observe("ssmcp.ui.message", {
            "text": "# Progress\n\nThe **agent is still working** while questions wait.\n\n"
                    "- A readable list item\n\n"
                    "Code stays literal:\n\n    touch questions-shell-leak"
        })
        self.check(shown["shown"] is True,
                   "an agent can display a progress message while questions are pending")
        front.until(lambda shot: any(
            "agent is still working" in row and "**" not in row for row in shot["rows"]
        ), "Markdown emphasis rendered as readable conversation text")
        self.check(not (self.workspace / "questions-shell-leak").exists(),
                   "Markdown code content is rendered without executing a shell command")
        self.save_shot("markdown", front)

        self.details(front, "DETAIL_OPTIONS_FIRST")
        self.save_shot("first", front)
        self.details(front, "DETAIL_FREE_SECOND")
        self.check(self.question(first["id"])["state"] == "pending"
                   and self.question(second["id"])["state"] == "pending",
                   "repeated Shift+Up changes questions without answering either one")
        front.send(ESC)
        front.until(lambda shot: column(shot) == ("questions", 5, 8) and shot["hide_cursor"],
                    "Escape returning from answer input to the question list")
        front.send(ESC)
        front.until(lambda shot: column(shot) == ("conversation", 2, 8) and not shot["hide_cursor"],
                    "a second Escape restoring the normal conversation")
        self.check(self.composition(self.ui("/display/conversation")) == original,
                   "two Escape transitions preserve normal text, cursor, recipient and edit identity")

        self.details(front, "DETAIL_OPTIONS_FIRST")
        front.send(b"2" + ENTER)
        answered = self.wait_question(front, first["id"], "answered")
        self.check(answered["answer"] == "HTML",
                   "a numbered suggested answer stores the option text under the original question ID")
        self.activity(front)
        self.check(self.composition(self.ui("/display/conversation")) == original,
                   "answering a choice does not publish or alter the normal message")
        self.details(front, "DETAIL_FREE_SECOND")
        answer = "界面保留草稿，先继续构建。"
        front.send(answer.encode("utf-8") + ENTER)
        answered = self.wait_question(front, second["id"], "answered")
        self.check(answered["answer"] == answer,
                   "a free-form Unicode answer reaches the native read tool byte for byte")
        self.activity(front)

        self.details(front, "DETAIL_SKIP_THIRD")
        front.send(ESC)
        front.until(lambda shot: column(shot) == ("questions", 5, 8) and shot["hide_cursor"],
                    "the optional question selected in the list")
        front.send(DELETE)
        skipped = self.wait_question(front, third["id"], "skipped")
        self.check(skipped["answer"] is None,
                   "Delete in the question list skips a question without fabricating an answer")
        self.activity(front)
        self.check(self.observe("ssmcp.ui.questions", {})["pending_count"] == 0,
                   "all three question outcomes remain readable after the pending list empties")
        self.check(self.composition(self.ui("/display/conversation")) == original,
                   "all question interactions preserve the same ordinary draft and exact target")

        # A server-confirmed request edit is also kept independent from the
        # question-answer input. The native queue calls are fixture operations.
        request = self.observe("ssmcp.ui.queue", {
            "action": "submit", "text": "QUESTION_EDIT_CONTEXT",
            "target": member["agent_id"],
        })
        request_id = request["id"]
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            queued = self.record(self.queue(), message_id=request_id)
            if queued["status"] == "queued":
                break
        else:
            raise AssertionError("fixture edit request was not acknowledged")
        self.observe("ssmcp.ui.queue", {"action": "takeback", "id": request_id})
        edited = self.wait_conversation(
            front, lambda state: state["editing"] == request_id,
            "the exact request was not reclaimed for editing",
        )
        edited = self.composition(edited)
        fourth = self.ask("Answer without publishing the held request.\n\nDETAIL_HELD_EDIT",
                          title="While editing")
        self.details(front, "DETAIL_HELD_EDIT")
        front.send(b"Keep editing" + ENTER)
        self.wait_question(front, fourth["id"], "answered")
        self.activity(front)
        self.check(self.composition(self.ui("/display/conversation")) == edited,
                   "answering a question preserves a held request's draft, target and original edit ID")
        self.check(self.record(self.queue(), message_id=request_id)["status"] == "editing",
                   "question Enter never republishes the normal request held for editing")

        self.native_rejected("ssmcp.ui.message", {"text": "INJECTION\x1b[2J\x1b[?1049l"},
                             "message contains unsupported control characters")
        self.native_rejected("ssmcp.ui.message", {"text": "Cannot impersonate", "role": "user"},
                             "message requires text and accepts kind and idempotency_key")
        front.pump(0.15)
        shot = front.snapshot()
        self.check(shot["alternate_screen"]
                   and "INJECTION" not in text(shot)
                   and self.composition(self.ui("/display/conversation")) == edited,
                   "rejected terminal escapes cannot clear the UI, leave the screen, or corrupt a draft")
        self.save_shot("complete", front)

        self.observe("ssmcp.ui.queue", {"action": "cancel", "id": request_id})
        final, restored = front.disconnect()
        self.check(not final["alternate_screen"] and restored == front.original_termios,
                   "the question scenario restores the owned terminal on exit")
        self.check(front.process.returncode == 0,
                   "the isolated question client exits successfully")
        self.metrics["questions"] = {
            "bytes": len(front.transcript),
            "sha256": hashlib.sha256(front.transcript).hexdigest(),
            "answered": 3, "skipped": 1,
        }
        front.close()


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("question E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--decoder", type=Path, help="defaults to BIN_DIR/examples/vt_snapshot")
    parser.add_argument("--report", type=Path, help="write assertions and hashes without credentials")
    parser.add_argument("--out", type=Path, help="write actual question and Markdown VT cell snapshots")
    args = parser.parse_args()
    decoder = args.decoder or args.bin_dir / "examples/vt_snapshot"
    for executable in (args.bin_dir / "ssmcp", args.bin_dir / "ssmcpd", decoder):
        if not executable.is_file():
            parser.error(f"missing {executable}; build the binaries and vt_snapshot first")
    started = time.monotonic()
    report = {
        "status": "failed", "platform": platform.system(),
        "transport": "isolated OpenSSH public-key + OAuth + owned PTY + independent vt100 decoder",
        "scope": "nonblocking questions, local answers, independent drafts, Markdown display; no models",
        "binaries": {name: sha256(path) for name, path in (
            ("ssmcp", args.bin_dir / "ssmcp"), ("ssmcpd", args.bin_dir / "ssmcpd"),
            ("vt_snapshot", decoder),
        )},
        "sources": {path.name: sha256(path) for path in (
            Path(__file__), Path(__file__).with_name("e2e.py"),
            Path(__file__).with_name("e2e_dashboard.py"),
            Path(__file__).with_name("e2e_client_ui.py"),
            Path(__file__).with_name("e2e_composer.py"),
        )},
    }
    with tempfile.TemporaryDirectory(prefix="ssmcp-questions-e2e-") as directory:
        harness = Questions(Path(directory), args.bin_dir, decoder, args.out)
        try:
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            description = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    description = description.replace(secret, "[redacted]")
            report["error"] = description
            print(description, flush=True)
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    if report["status"] != "passed":
        raise SystemExit(1)
    print(f"\nPassed {len(report['checks'])} question checks in {report['duration_seconds']}s", flush=True)


if __name__ == "__main__":
    main()
