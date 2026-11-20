#!/usr/bin/env python3
"""Verify next-interaction UI through owned PTYs and real authenticated loopback services.

Requires prebuilt, immutable product/fixture binaries and an independent VT decoder.
No Cargo, external model, existing account, live client, desktop, or X server is used.
"""

import argparse
import concurrent.futures
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time

from e2e_client_ui import body_text, prompt_text
from e2e_composer import DOWN, ENTER, ESC, HOME, LEFT, RIGHT, UP
from e2e_dashboard import text
from e2e_input_history import InputHistory, PASTE_START, PASTE_END

SHIFT_UP = b"\x1b[1;2A"
DELETE = b"\x1b[3~"


def digest(path):
    with Path(path).open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


class Interactions(InputHistory):
    def __init__(self, directory, binary_dir, decoder, output):
        super().__init__(directory, binary_dir, decoder, output)
        self.frames = []
        self.quiet_checks = []

    def start_front(self, label):
        front = super().start_front(label)
        front.input_bytes = 0
        send = front.send

        def counted(data):
            front.input_bytes += len(data.encode() if isinstance(data, str) else data)
            send(data)

        front.send = counted
        front.resize(32, 120)
        front.pump(0.15)
        return front

    def send_stream(self, front, data):
        front.input_bytes += len(data)
        super().send_stream(front, data)

    def quiet_until(self, front, predicate, description, *, timeout=9):
        # Only pump this test's PTY/decoder. No UI observer call or keyboard input
        # may create the frame that this assertion attributes to an async reply.
        before = front.input_bytes
        shot = front.until(predicate, description, timeout=timeout)
        self.check(front.input_bytes == before,
                   description + ": no keyboard input was sent during the observation")
        self.quiet_checks.append(description)
        return shot

    def capture_pair(self, front, name, marker, *, rows=32):
        for columns in (120, 48):
            front.resize(rows, columns)
            front.pump(0.15)
            shot = front.until(
                lambda value: value["size"] == [rows, columns] and marker in text(value),
                name + " at " + str(columns) + " columns", cells=True)
            stem = "interactions-" + name + "-" + str(columns)
            (self.output / (stem + ".json")).write_text(
                json.dumps(shot, ensure_ascii=False, indent=2) + "\n")
            (self.output / (stem + ".txt")).write_text(text(shot) + "\n")
            self.frames.append(stem + ".json")
        front.resize(rows, 120)
        front.pump(0.15)

    def conversation(self, front):
        for _ in range(4):
            state = self.ui("/display/conversation")
            if state["view"] == "activity":
                self.focus_input(front)
                return self.wait_state(
                    front, lambda item: item["view"] == "activity" and item["focus"] == "input",
                    "return to the retained conversation draft")
            front.send(ESC)
            front.pump(0.12)
        raise AssertionError("Escape did not return the owned UI to its conversation")

    def replace_short(self, front, value):
        if len(value.encode()) > 4096 or len(value.splitlines()) >= 8:
            raise AssertionError("the synthetic draft helper only accepts inline input")
        self.conversation(front)
        before = self.ui("/display/conversation")["draft"]
        raw = before["text"].encode()
        prefix = raw[:before["cursor"]].decode()
        suffix = raw[before["cursor"]:].decode()
        self.send_stream(front, b"\x7f" * len(prefix) + DELETE * len(suffix))
        self.wait_state(front, lambda item: item["draft"]["text"] == "",
                        "clear only the owned synthetic draft")
        if value:
            self.send_stream(front, PASTE_START + value.encode() + PASTE_END)
        return self.wait_state(
            front, lambda item: item["draft"]["text"] == value,
            "replace the owned short draft without changing its route")

    def open_picker(self, front, query=""):
        self.replace_short(front, "/message" + (" " + query if query else ""))
        front.send(ENTER)
        return self.wait_state(front, lambda item: item["view"] == "agents",
                               "the explicit native recipient picker")

    def messages_call(self, action, client_id, arguments=None, *, agent=None):
        result = self.response("return await messages.%s(%s, %s);" % (
            action, json.dumps(client_id), json.dumps(arguments or {})), agent=agent)
        reply = result["structuredContent"]["result"]
        if not isinstance(reply, dict) or reply.get("isError"):
            raise AssertionError("native messages helper did not return a successful typed reply")
        return reply

    def binding(self, member):
        return {"client_id": self.client_id, "id": member["agent_id"], "token": member["token"]}

    def scoped_native(self, member, name, arguments):
        result = self.response("return await call(%s, %s, %s);" % (
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments)),
            agent=self.binding(member))
        native = result["structuredContent"]["result"]
        if native.get("isError") or not isinstance(native.get("structuredContent"), dict):
            raise AssertionError("the authenticated native UI request failed: " + name)
        return native["structuredContent"]

    def question(self, member, request_id):
        return self.scoped_native(member, "ssmcp.ui.questions", {"id": request_id})["question"]

    def open_question(self, front, request_id):
        front.send(SHIFT_UP)
        return self.wait_state(
            front, lambda item: item["view"] == "questions" and item["answering"] == request_id,
            "Shift+Up opening the exact pending request")

    def pending_recall(self, front):
        delivered_text = "UP_ALREADY_DELIVERED"
        original = "UP_PENDING_ORIGINAL 中文"
        revised = "UP_EDITED_SENT 中文"
        markers = (delivered_text, "UP_PENDING_ORIGINAL", "UP_EDITED_SENT")
        self.conversation(front)
        front.resize(58, 120)
        front.pump(0.15)
        initial = self.ui("/display/conversation")
        before_ids = {item["id"] for item in self.queue()["queue"]}
        self.check(initial["draft"]["text"] == "" and initial["target"] is None
                   and initial["native_recipient"] is None,
                   "pending recall starts with an empty next-caller composer")

        def queue_state(record, status, source):
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline:
                front.pump(0.05)
                current = self.record(self.queue(), message_id=record["id"])
                if current["status"] == status and current["text"] == source:
                    return current
            raise AssertionError("the exact recall fixture did not become " + status)

        def frames(name, expected, draft_marker=None):
            self.capture_pair(front, "pending-recall-" + name, delivered_text, rows=58)
            for filename in self.frames[-2:]:
                shot = json.loads((self.output / filename).read_text())
                body, prompt = body_text(shot), prompt_text(shot)
                self.check(tuple(body.count(marker) for marker in markers) == expected,
                           name + ": real conversation has exact old/new/delivered body counts at "
                           + str(shot["size"][1]) + " columns")
                self.check(all(prompt.count(marker) == int(marker == draft_marker)
                               for marker in markers),
                           name + ": recalled text appears only in its intended physical composer")

        delivered = self.submit_source(front, delivered_text)
        self.expect_delivery(self.work(), delivered)
        queue_state(delivered, "delivered", delivered_text)
        pending = self.submit_source(front, original)
        expected_ids = before_ids | {delivered["id"], pending["id"]}
        self.quiet_until(
            front, lambda shot: body_text(shot).count(markers[0]) == 1
            and body_text(shot).count(markers[1]) == 1 and markers[1] not in prompt_text(shot),
            "the acknowledged pending bubble is visible before Up")
        frames("before-up", (1, 1, 0))

        front.send(UP)
        self.quiet_until(
            front, lambda shot: original in prompt_text(shot)
            and markers[1] not in body_text(shot) and body_text(shot).count(markers[0]) == 1,
            "server-confirmed Up recall removes only the old pending bubble without another key")
        recalled = self.ui("/display/conversation")
        expected_recall = (
            pending["id"],
            None if pending["target"] == "next" else pending["target"],
            {"text": original, "cursor": len(original.encode())},
        )
        observed_recall = (recalled["editing"], recalled["target"], recalled["draft"])
        recall_matches = observed_recall == expected_recall
        self.check(
            recall_matches,
            "Up restores the exact request identity, source and UTF-8 end cursor"
            + ("" if recall_matches else
               f"; observed={observed_recall!r}; expected={expected_recall!r}"),
        )
        queue_state(pending, "editing", original)
        frames("recalled", (1, 0, 0), markers[1])

        # Escape dismisses the notice; it does not cancel or republish an edit.
        # Exercise a non-end UTF-8 cursor so preservation is not a default value.
        front.send(LEFT)
        cursor = len(original[:-1].encode())
        before_escape = self.wait_state(
            front, lambda state: state["draft"]["text"] == original and state["draft"]["cursor"] == cursor,
            "move the recalled cursor left across one Chinese character")
        front.send(ESC)
        front.pump(0.12)
        escaped = self.ui("/display/conversation")
        self.check(escaped["draft"] == before_escape["draft"] and escaped["editing"] == pending["id"]
                   and escaped["target"] == recalled["target"],
                   "Escape preserves the held edit, its exact text, cursor and recipient")
        queue_state(pending, "editing", original)
        self.check(not self.messages(self.work()),
                   "the held edit is not delivered and the earlier delivered message is not replayed")
        self.check({item["id"] for item in self.queue()["queue"]} == expected_ids,
                   "recall and Escape do not create or cancel a replacement request")
        frames("escape-held", (1, 0, 0), markers[1])

        self.replace_short(front, revised)
        front.send(ENTER)
        self.quiet_until(
            front, lambda shot: body_text(shot).count(markers[2]) == 1
            and markers[1] not in body_text(shot) and body_text(shot).count(markers[0]) == 1
            and markers[2] not in prompt_text(shot),
            "resubmitting the edit adds exactly one new bubble and keeps the delivered bubble")
        edited = queue_state(pending, "queued", revised)
        accepted = self.ui("/display/conversation")
        self.check(edited["revision"] > pending["revision"] and edited["target"] == pending["target"]
                   and accepted["editing"] is None and accepted["draft"]["text"] == "",
                   "resubmission advances the same request revision and clears only its accepted edit")
        self.check({item["id"] for item in self.queue()["queue"]} == expected_ids,
                   "resubmission reuses the original message ID without an additional request")
        frames("resubmitted", (1, 0, 1))
        self.expect_delivery(self.work(), pending, request_text=revised)
        queue_state(pending, "delivered", revised)
        self.check(not self.messages(self.work()), "the revised request is delivered once without replay")

        # With no queued request, Up is ordinary input-history navigation.
        # Its copied text must not retract either already delivered chat card.
        self.check(self.record(self.queue(), message_id=delivered["id"])["status"] == "delivered",
                   "the earlier delivered request remains delivered throughout pending editing")
        before_history = {item["id"]: {key: item[key] for key in ("text", "status", "revision", "target")}
                          for item in self.queue()["queue"]}
        front.send(UP)
        self.quiet_until(
            front, lambda shot: revised in prompt_text(shot)
            and body_text(shot).count(markers[0]) == 1 and body_text(shot).count(markers[2]) == 1
            and markers[1] not in body_text(shot),
            "Up after delivery copies history while retaining the delivered conversation bodies")
        history = self.ui("/display/conversation")
        self.check(history["editing"] is None
                   and history["draft"] == {"text": revised, "cursor": len(revised.encode())},
                   "history recall of delivered text is an ordinary draft, not a queue takeback")
        after_history = {item["id"]: {key: item[key] for key in ("text", "status", "revision", "target")}
                         for item in self.queue()["queue"]}
        self.check(after_history == before_history,
                   "ordinary history recall cannot change delivered queue identities, revisions or states")
        frames("delivered-history", (1, 0, 1), markers[2])
        self.replace_short(front, "")
        front.resize(32, 120)
        front.pump(0.15)
        return {"request_id": pending["id"], "preserved_delivered_id": delivered["id"],
                "original_revision": pending["revision"], "edited_revision": edited["revision"],
                "verified_widths": [120, 48], "rows": 58, "escape_preserves_edit": True, "delivered_once": True}

    def command_details(self, front):
        secret = "command-visibility-fixture-private"
        self.sensitive.append(secret)
        counter = self.workspace / "command-view-once.txt"
        jobs_before = {item["job_id"] for item in self.call("ssmcp.exec.list")["jobs"]}
        script = (
            "import sys\n"
            "from pathlib import Path\n"
            "p=Path('command-view-once.txt')\n"
            "p.write_text((p.read_text() if p.exists() else '') + 'once\\n')\n"
            "print('COMMAND_VIEW_EXECUTED_ONCE', flush=True)\n"
            "# API_TOKEN=" + secret + "\n"
            "# " + "界" * 700 + "\n"
            "sys.stdin.read()\n"
            "# COMMAND_SOURCE_END\n"
        )
        argv = ["python3", "-c", script, "", "COMMAND_ARGV_LAST"]
        # Keep only this harmless fixture alive so inspection can prove that it
        # neither cancels the selected job nor creates a replacement execution.
        job = self.call("ssmcp.exec.start", {
            "command": argv, "cwd": str(self.workspace), "stdin": "pipe", "timeout_ms": 90000})
        deadline = time.monotonic() + 8
        while not counter.exists() and time.monotonic() < deadline:
            front.pump(0.05)
        self.check(counter.exists() and counter.read_text() == "once\n",
                   "the owned command reached its private stdin wait exactly once")
        before = self.call("ssmcp.exec.status", {"job_id": job["job_id"]})
        self.check(before["running"] is True and before["cancelled"] is False,
                   "the native command is running before opening its details")
        self.replace_short(front, "/command " + job["job_id"])
        front.send(ENTER)
        self.quiet_until(
            front, lambda shot: "argv[0]" in text(shot) and "State: running" in text(shot),
            "native command details arrive without keyboard input or UI observation")
        state = self.ui("/display/conversation")
        self.check(state["view"] == "tasks" and state["tasks"]["source"] == "native"
                   and state["tasks"]["loaded"] is True and state["tasks"]["detail"]
                   and state["tasks"]["selected_id"] == job["job_id"]
                   and isinstance(state["tasks"]["selected_job"], dict),
                   "the completed asynchronous read selects the exact real native job")
        self.check(state["tasks"]["selected_job"]["command"] == argv,
                   "display redaction does not modify native command arguments")

        def save_frame(name, columns, shot):
            encoded = json.dumps(shot, ensure_ascii=False, indent=2) + "\n"
            self.check(secret not in encoded,
                       f"{name} frame contains no fixture secret at {columns} columns")
            stem = f"interactions-{name}-{columns}"
            (self.output / (stem + ".json")).write_text(encoded)
            (self.output / (stem + ".txt")).write_text(text(shot) + "\n")
            self.frames.append(stem + ".json")

        for columns in (120, 48):
            front.resize(32, columns)
            front.pump(0.15)
            front.send(HOME)
            shot = front.until(lambda item: "argv[0]" in text(item),
                               "the first exact argument is visible", cells=True)
            save_frame("command-start", columns, shot)
            redacted_seen = empty_seen = source_end_seen = False
            for _ in range(200):
                shot = front.snapshot(cells=True)
                visible = text(shot)
                self.check(secret not in visible,
                           f"command details keep the fixture secret redacted at {columns} columns")
                redacted_seen |= "API_TOKEN=[redacted]" in visible
                empty_seen |= 'argv[3]: ""' in visible
                source_end_seen |= "COMMAND_SOURCE_END" in visible
                if "COMMAND_ARGV_LAST" in visible:
                    break
                front.send(DOWN)
                front.pump(0.025)
            else:
                raise AssertionError("scrolling never reached the last complete argument")
            self.check(redacted_seen,
                       f"the actual token assignment is visibly redacted at {columns} columns")
            self.check(empty_seen,
                       f"the empty argument remains explicit at {columns} columns")
            self.check(source_end_seen,
                       f"long multiline source reaches its real final line at {columns} columns")
            save_frame("command-end", columns, shot)

        # Tasks Escape returns to its list/input, not to the conversation column.
        front.resize(32, 120)
        front.pump(0.15)
        self.activate_column(front, "conversation")
        self.conversation(front)
        missing = "not-retained-" + job["job_id"]
        self.replace_short(front, "/command " + missing)
        front.send(ENTER)
        self.quiet_until(
            front, lambda shot: "isnolongerretained." in "".join(text(shot).split()),
            "an exact missing command ID renders its own unavailable result")
        state = self.ui("/display/conversation")
        self.check(state["view"] == "tasks" and state["tasks"]["detail"]
                   and state["tasks"]["selected_id"] == missing
                   and state["tasks"]["selected_job"] is None,
                   "a missing exact ID never falls back to another retained task")
        for columns in (120, 48):
            front.resize(32, columns)
            shot = front.until(
                lambda item: item["size"] == [32, columns]
                and "isnolongerretained." in "".join(text(item).split()),
                f"the missing command result at {columns} columns", cells=True)
            self.check("COMMAND_ARGV_LAST" not in text(shot)
                       and "COMMAND_SOURCE_END" not in text(shot),
                       f"the missing-ID pane contains no other job's script at {columns} columns")
            save_frame("command-missing", columns, shot)

        jobs_after = {item["job_id"] for item in self.call("ssmcp.exec.list")["jobs"]}
        after = self.call("ssmcp.exec.status", {"job_id": job["job_id"]})
        self.check(jobs_after == jobs_before | {job["job_id"]},
                   "opening, scrolling and unknown-ID lookup create no native jobs")
        self.check(all(after[key] == before[key]
                       for key in ("job_id", "pid", "command", "cwd", "state", "running",
                                   "exit_code", "exit_signal", "cancelled", "timed_out"))
                   and after["running"] is True and after["cancelled"] is False,
                   "read-only command navigation preserves the running native task")
        self.check(counter.read_text() == "once\n",
                   "opening and scrolling command details never re-executes the command")
        self.call("ssmcp.exec.write", {"job_id": job["job_id"], "close_stdin": True})
        finished = self.finish_job(job, front)
        self.check(finished["exit_code"] == 0 and finished["cancelled"] is False,
                   "only fixture-owned stdin EOF finishes the inspected command normally")
        front.resize(32, 120)
        self.activate_column(front, "conversation")
        self.conversation(front)

    def ordinary_question(self, front, requester):
        draft = "MAIN_DRAFT 中文和路径 @./keep-local.txt"
        self.replace_short(front, draft)
        created = self.scoped_native(requester, "ssmcp.ui.ask", {
            "question": "请选择下一步，并保留当前主草稿。",
            "title": "PTY ordinary question", "idempotency_key": "pty-ordinary"})
        self.check(created["blocking"] is False and created["automatic_delivery"] is True,
                   "a verified ordinary question returns immediately with exact-thread delivery")
        request_id = created["id"]
        self.check(not self.ui("/display/sleeping"),
                   "an asynchronous ordinary question does not put the session to sleep")
        self.open_question(front, request_id)
        answer = "第一行 answer\n  第二行保留缩进"
        self.send_stream(front, PASTE_START + answer.encode() + PASTE_END)
        front.send(ENTER)
        front.until(lambda shot: "Answer queued" in text(shot),
                    "the keyboard answer's pending delivery receipt")
        view = self.question(requester, request_id)
        self.check(view["answer"] == answer and view["delivery"] == "pending"
                   and view["answer_actor"]["origin"] == "human",
                   "local answer records actual human origin and remains pending before requester work")
        self.check(self.ui("/display/conversation")["draft"]["text"] == draft,
                   "answering a question leaves the main draft byte-for-byte unchanged")
        self.capture_pair(front, "answer-pending", "Answer queued")
        reply = self.response("return null;", agent=self.binding(requester))
        answers = reply["structuredContent"].get("question_answers", [])
        self.check(len(answers) == 1 and answers[0]["id"] == request_id
                   and answers[0]["answer"] == answer
                   and answers[0]["delivery"] == "delivered"
                   and answers[0]["source"] == "question_answer",
                   "the requester's next independent bound call receives the exact typed answer")
        self.quiet_until(front, lambda shot: "Included in requester tool response" in text(shot),
                         "answer delivery receipt refreshes without a key or UI observation call")
        self.capture_pair(front, "answer-included", "Included in requester")
        self.conversation(front)
        return draft

    def create_finish(self, requester, suffix):
        result = self.public_call("request_finish", {
            "client_id": self.client_id, "reason": "PTY finish review " + suffix,
            "timeout_ms": 1000, "idempotency_key": "pty-finish-" + suffix,
            "agent": {"id": requester["agent_id"], "token": requester["token"]}})
        self.check(result["timed_out"] is True and result["decision"] is None
                   and result["approved"] is False and result["default_decision"] == "no",
                   suffix + ": bounded waiting leaves an undecided request with default No")
        return result

    def finish_decision(self, front, requester, choice, comment, draft):
        initial = self.create_finish(requester, choice)
        request_id = initial["request_id"]
        arguments = {
            "client_id": self.client_id, "reason": "PTY finish review " + choice,
            "timeout_ms": 15000, "request_id": request_id,
            "agent": {"id": requester["agent_id"], "token": requester["token"]}}
        with concurrent.futures.ThreadPoolExecutor(max_workers=1) as executor:
            waiting = executor.submit(self.public_call, "request_finish", arguments)
            front.until(lambda shot: "Click or type to wake" in text(shot),
                        "a real native finish waiter sleeping the actual terminal")
            if choice == "no":
                self.capture_pair(front, "finish-sleep", "Click or type to wake")
            # The complete real SGR gesture only dismisses sleep. It cannot choose
            # a decision, edit a draft, or synthesize a human answer.
            front.send(b"\x1b[<0;4;8M\x1b[<0;4;8m")
            front.until(lambda shot: "Click or type to wake" not in text(shot),
                        "the finish sleep overlay dismissed by one complete click")
            self.check(not waiting.done()
                       and self.question(requester, request_id)["finish"]["decision"] is None
                       and self.ui("/display/conversation")["draft"]["text"] == draft,
                       choice + ": sleep dismissal preserves the pending request and original draft")
            self.open_question(front, request_id)
            front.until(lambda shot: "● No" in text(shot), "the explicit default No choice")
            if choice == "yes":
                shot = front.snapshot()
                row, line = next((index, line) for index, line in enumerate(shot["rows"])
                                 if "○ Yes" in line)
                column = line.index("Yes") + 1
                front.send(("\x1b[<0;%d;%dM\x1b[<0;%d;%dm" %
                            (column, row + 1, column, row + 1)).encode())
                front.until(lambda shot: "● Yes" in text(shot),
                            "a real mouse gesture explicitly selecting Yes")
            front.send(b"\t")
            self.send_stream(front, PASTE_START + comment.encode() + PASTE_END)
            self.capture_pair(front, "finish-selected-" + choice,
                              "● " + choice.capitalize())
            front.send(ENTER)
            result = waiting.result(timeout=8)
        decision = result["decision"]
        self.check(result["timed_out"] is False and result["approved"] is (choice == "yes")
                   and decision["choice"] == choice and decision["comment"] == comment
                   and decision["actor"]["origin"] == "human",
                   choice + ": Enter records only the explicitly selected decision and separate comment")
        self.check(self.ui("/display/conversation")["draft"]["text"] == draft,
                   choice + ": the real approval handler completes without losing the main draft")
        if choice == "no":
            self.check(comment == "yes" and result["approved"] is False,
                       "ordinary comment text yes never changes default No into approval")
        self.capture_pair(front, "finish-recorded-" + choice,
                          "No · keep working" if choice == "no" else "Approved")
        if choice == "yes":
            final = {"kind": "answer", "text": "PTY approved completion receipt",
                     "idempotency_key": "pty-approved-final"}
            receipt = self.scoped_native(requester, "ssmcp.ui.message", final)
            duplicate = self.scoped_native(requester, "ssmcp.ui.message", final)
            self.check(receipt["accepted"] is True and receipt["duplicate"] is False
                       and receipt["finish_request_id"] == request_id
                       and duplicate["duplicate"] is True
                       and duplicate["message_id"] == receipt["message_id"],
                       "the approved real final answer consumes one grant and matching retries reuse its receipt")
            self.check(self.question(requester, request_id)["finish"]["status"] == "completed",
                       "the final receipt closes this finish request before another is created")
            self.capture_pair(front, "finish-completed", "Finish receipt committed")
        self.conversation(front)

    def external_finish_decision(self, front, requester, reviewer, draft):
        initial = self.create_finish(requester, "external")
        request_id = initial["request_id"]
        self.open_question(front, request_id)
        front.send(b"\t")
        self.send_stream(front, PASTE_START + "human note not sent".encode() + PASTE_END)
        response = self.scoped_native(reviewer, "ssmcp.ui.answer", {
            "id": request_id, "decision": "no", "comment": "reviewer asks for one more check"})
        self.check(isinstance(response, dict), "a second verified agent returns a typed finish receipt")
        self.quiet_until(
            front, lambda shot: "Agent PTY reviewer" in text(shot)
            and "Your unsent draft" in text(shot) and "human note not sent" in text(shot),
            "another verified agent's decision appears while retaining the unsent local comment")
        view = self.question(requester, request_id)
        actor = view["finish"]["decision"]["actor"]
        self.check(actor["origin"] == "agent"
                   and actor["agent"]["thread_id"] == reviewer["agent_id"]
                   and view["finish"]["decision"]["choice"] == "no",
                   "the displayed external decision is bound to the real other agent identity")
        self.check(self.ui("/display/conversation")["draft"]["text"] == draft,
                   "an external decision never replaces the main draft")
        self.capture_pair(front, "finish-external", "Your unsent draft")
        self.conversation(front)

    def native_message(self, front):
        self.replace_short(front, "")
        self.open_picker(front)
        # No message has been sent. The picker must watch recipients while idle;
        # an outgoing message is deliberately not used to enable background polling.
        before = front.input_bytes
        process, credentials = self.start_client(
            "native-late", name="PTY-native-recipient", cwd=self.workspace)
        destination = credentials["client_id"]
        self.check(process.poll() is None, "the late recipient is a real owned ordinary client")
        self.quiet_until(front, lambda shot: "PTY-native-recipient" in text(shot),
                         "an idle picker shows a newly connected recipient without a key")
        self.check(front.input_bytes == before,
                   "starting the recipient does not inject input into the source PTY")
        recipients = self.messages_call("list", self.client_id)["recipients"]
        descriptor = next(item for item in recipients if item["id"] == "client:" + destination)
        self.check(descriptor["kind"] == "account_inbox" and descriptor["agent_id"] is None
                   and descriptor["online"] is True and descriptor["sendable"] is True,
                   "the picker target is an authenticated account inbox, not a fabricated model thread")
        self.capture_pair(front, "native-idle-picker", "PTY-native-recipient")
        body = "!touch .native-must-not-execute\n@./missing-native-file\n  消息原文"
        original = "@" + descriptor["display_name"] + " " + body
        self.replace_short(front, original)
        queue_before = self.queue()["queue"]
        front.send(ENTER)
        state = self.wait_state(front, lambda item: item["view"] == "agents",
                                "a known mention opening an explicit picker")
        self.check(state["native_recipient"] is None and state["draft"]["text"] == original,
                   "typing a known recipient mention retains the draft and does not select or send")
        self.check(not self.messages_call("inbox", destination)["messages"],
                   "the recipient receives nothing before explicit selection and send")
        front.send(ENTER)
        state = self.wait_state(front, lambda item: item["view"] == "activity"
                                and item["native_recipient"] == descriptor["id"],
                                "Enter selecting the exact authenticated recipient")
        self.check(state["draft"]["text"] == original,
                   "explicit recipient selection preserves all draft bytes")
        front.send(ENTER)
        self.quiet_until(front, lambda shot: "Queued by service" in shot["rows"][-1],
                         "the service's queued receipt replaces local submitting without another key")
        state = self.ui("/display/conversation")
        self.check(state["draft"]["text"] == "" and state["native_recipient"] == descriptor["id"],
                   "only the matching service receipt clears the sent draft and retains its recipient")
        self.check(self.queue()["queue"] == queue_before
                   and not (self.workspace / ".native-must-not-execute").exists(),
                   "native text neither enters the ordinary requirement queue nor executes a shell command")
        reply = self.response("return await call(%s, 'terminal.status', {});" % json.dumps(destination))
        messages = reply["structuredContent"].get("native_messages", [])
        self.check(len(messages) == 1 and messages[0]["text"] == body
                   and messages[0]["recipient"]["id"] == descriptor["id"]
                   and messages[0]["sender"]["kind"] == "local_user",
                   "the selected account's next real call receives unchanged text with local-user origin")
        self.quiet_until(front, lambda shot: "Included in recipient tool response" in shot["rows"][-1],
                         "queued changes to included solely after the recipient's remote call")
        message_id = messages[0]["receipt"]["message_id"]
        acknowledgement = self.messages_call("ack", destination, {"message_id": message_id})
        receipts = acknowledgement.get("receipts", [])
        self.check(len(receipts) == 1 and receipts[0]["message_id"] == message_id
                   and receipts[0]["recipient_id"] == descriptor["id"]
                   and receipts[0]["status"] == "acknowledged",
                   "the account inbox acknowledges this exact message and recipient")
        self.quiet_until(front, lambda shot: "Acknowledged by recipient" in shot["rows"][-1],
                         "explicit recipient acknowledgement refreshes the source without input")
        self.capture_pair(front, "native-acknowledged", "Acknowledged")

        self.open_picker(front)
        member = self.public_call("collab", {
            "action": "join", "client_id": destination,
            "name": "PTY-native-agent", "role": "receiver"})
        self.sensitive.append(member["token"])
        self.quiet_until(front, lambda shot: "PTY-native-agent" in text(shot),
                         "the idle picker also refreshes a newly verified agent without a key")
        agent_id = "agent:" + destination + ":" + member["agent_id"]
        self.open_picker(front, agent_id)
        state = self.ui("/display/conversation")
        self.check(state["selected_agent"] == "native:" + agent_id
                   and state["native_recipient"] == descriptor["id"],
                   "highlighting a verified agent does not replace the selected account inbox")
        self.capture_pair(front, "native-agent-picker", "PTY-native-agent")
        front.send(ENTER)
        self.wait_state(front, lambda item: item["view"] == "activity"
                        and item["native_recipient"] == agent_id,
                        "explicitly selecting the exact verified native agent")
        agent_text = "VERIFIED_AGENT_ONLY 中文原文"
        self.replace_short(front, agent_text)
        front.send(ENTER)
        self.quiet_until(front, lambda shot: "Queued by service" in shot["rows"][-1],
                         "a verified-agent send obtains its own service receipt without another key")
        wrong = self.response("return await call(%s, 'terminal.status', {});" % json.dumps(destination))
        self.check(not wrong["structuredContent"].get("native_messages"),
                   "an unbound call on the same account and device cannot take an exact-agent message")
        binding = {"client_id": destination, "id": member["agent_id"], "token": member["token"]}
        exact = self.response("return await call(%s, 'terminal.status', {});" % json.dumps(destination),
                              agent=binding)
        received = exact["structuredContent"].get("native_messages", [])
        self.check(len(received) == 1 and received[0]["text"] == agent_text
                   and received[0]["recipient"]["id"] == agent_id
                   and received[0]["recipient"]["kind"] == "agent",
                   "only the selected verified agent receives the message through its actual bound call")
        self.quiet_until(front, lambda shot: "Included in recipient tool response" in shot["rows"][-1],
                         "the verified-agent delivery updates the source without input")
        agent_message_id = received[0]["receipt"]["message_id"]
        acknowledgement = self.messages_call("ack", destination,
                                             {"message_id": agent_message_id}, agent=binding)
        receipts = acknowledgement.get("receipts", [])
        self.check(len(receipts) == 1 and receipts[0]["message_id"] == agent_message_id
                   and receipts[0]["recipient_id"] == agent_id
                   and receipts[0]["status"] == "acknowledged",
                   "the verified agent acknowledges this exact message and recipient")
        self.quiet_until(front, lambda shot: "Acknowledged by recipient" in shot["rows"][-1],
                         "the exact agent's acknowledgement refreshes independently")
        return agent_id

    def private_pastes(self, front, recipient_id):
        boundary = "x" * 4093 + "中"
        self.replace_short(front, boundary)
        self.check(len(boundary.encode()) == 4096
                   and self.ui("/display/conversation")["draft"]["text"] == boundary,
                   "exactly 4 KiB remains inline without cutting a UTF-8 character")
        self.replace_short(front, "single-line byte limit")
        over = ("x" * 4094 + "中").encode()
        self.send_stream(front, PASTE_START + over + PASTE_END)
        stored = self.wait_state(
            front, lambda item: '@".ssmcp/inputs/' in item["draft"]["text"],
            "a single UTF-8 line above 4 KiB becoming a private reference")
        byte_reference = re.search(r'@"(\.ssmcp/inputs/paste-[0-9a-f]+\.txt)"',
                                   stored["draft"]["text"])
        self.check(len(over) == 4097 and byte_reference is not None
                   and (self.workspace / byte_reference.group(1)).read_bytes() == over,
                   "4 KiB plus one byte is stored whole even when it has only one logical line")
        seven = "\n".join("line " + str(index) for index in range(7))
        self.replace_short(front, seven)
        self.check(self.ui("/display/conversation")["draft"]["text"] == seven,
                   "seven short logical lines remain inline with their newlines")
        self.replace_short(front, "paste prefix suffix")
        front.send(HOME + RIGHT * len("paste prefix"))
        raw = ("中文\r\n  indented\rkept\n" * 4).encode()
        self.send_stream(front, PASTE_START + raw + PASTE_END)
        state = self.wait_state(
            front, lambda item: '@".ssmcp/inputs/' in item["draft"]["text"],
            "eight or more logical lines becoming a private paste reference")
        match = re.search(r'@"(\.ssmcp/inputs/paste-[0-9a-f]+\.txt)"', state["draft"]["text"])
        self.check(match is not None and state["draft"]["text"].startswith("paste prefix")
                   and state["draft"]["text"].endswith(" suffix")
                   and state["native_recipient"] == recipient_id,
                   "automatic paste storage preserves the surrounding draft and exact native recipient")
        saved = self.workspace / match.group(1)
        self.check(saved.read_bytes() == raw and saved.stat().st_mode & 0o077 == 0,
                   "the private paste file preserves every original UTF-8, CR and LF byte")
        self.capture_pair(front, "long-paste", ".ssmcp/inputs/")

        self.replace_short(front, "recoverable original draft")
        before = self.ui("/display/conversation")
        inputs = self.workspace / ".ssmcp" / "inputs"
        held = self.workspace / ".ssmcp" / "inputs-owned-test-backup"
        self.check(inputs.is_dir() and not held.exists(),
                   "the failure fixture only replaces this test's own private input directory")
        inputs.rename(held)
        try:
            inputs.write_text("owned non-directory blocker")
            inputs.chmod(0o600)
            original = ("RECOVER 中文\r\n  exact content\n" * 170).encode()
            self.send_stream(front, PASTE_START + original + PASTE_END)
            failed = self.wait_state(
                front, lambda item: item["retained_paste_bytes"] == len(original),
                "a failed private write retaining the original paste")
            self.check(failed["draft"] == before["draft"]
                       and failed["native_recipient"] == recipient_id,
                       "a real storage error preserves the original draft, cursor and selected recipient")
            self.capture_pair(front, "paste-retained", "Original paste retained")
        finally:
            if inputs.is_file():
                inputs.unlink()
            held.rename(inputs)
        # Replacing this synthetic draft with an explicit retry command is a
        # deliberate test input. The retained paste is never reconstructed from
        # visible, truncated terminal text or injected through a test-only API.
        self.replace_short(front, "/paste-retry")
        front.send(ENTER)
        restored = self.wait_state(
            front, lambda item: item["retained_paste_bytes"] is None
            and '@".ssmcp/inputs/' in item["draft"]["text"],
            "the explicit keyboard retry storing the original retained bytes")
        reference = re.search(r'@"(\.ssmcp/inputs/paste-[0-9a-f]+\.txt)"',
                              restored["draft"]["text"])
        self.check(reference is not None
                   and (self.workspace / reference.group(1)).read_bytes() == original
                   and restored["native_recipient"] == recipient_id,
                   "retry saves the complete retained source and preserves the exact native route")
        self.capture_pair(front, "paste-retried", ".ssmcp/inputs/")
        self.check(not (self.workspace / ".native-must-not-execute").exists(),
                   "paste and message tests did not execute their literal shell-looking content")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(
            ["git", "-c", "init.defaultBranch=main", "init", "-q"], cwd=self.workspace,
            env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5,
        )
        front = self.start_front("PTY-interactions")
        recalled = self.pending_recall(front)
        requester = self.public_call("collab", {
            "action": "join", "client_id": self.client_id, "name": "PTY requester", "role": "requester"})
        reviewer = self.public_call("collab", {
            "action": "join", "client_id": self.client_id, "name": "PTY reviewer", "role": "reviewer"})
        self.sensitive.extend([requester["token"], reviewer["token"]])
        self.command_details(front)
        draft = self.ordinary_question(front, requester)
        self.finish_decision(front, requester, "no", "yes", draft)
        self.finish_decision(front, requester, "yes", "ready after explicit review", draft)
        self.external_finish_decision(front, requester, reviewer, draft)
        recipient_id = self.native_message(front)
        self.private_pastes(front, recipient_id)
        return {
            "status": "passed", "checks": len(self.checks), "passed_checks": self.checks,
            "frames": self.frames, "quiet_checks": self.quiet_checks,
            "pending_recall": recalled,
            "transport": "real production account router, native tools, WebSocket and owned PTY",
            "upstream_provider": "synthetic GitHub fixture",
            "models_started": False, "existing_clients_reloaded": False,
            "existing_services_touched": False, "real_user_credentials_used": False,
            "source_of_images": "actual owned PTY decoded by the independent VT fixture",
        }


def interrupted(_signum, _frame):
    raise KeyboardInterrupt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--decoder", required=True, type=Path)
    parser.add_argument("--fixture-binary", required=True, type=Path)
    parser.add_argument("--source-revision", required=True)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    if not re.fullmatch(r"[0-9a-f]{40}", args.source_revision):
        parser.error("--source-revision must be a complete frozen source SHA")
    binaries = {name: args.bin_dir.resolve() / name for name in ("ssmcp", "ssmcpd")}
    binaries.update(decoder=args.decoder.resolve(), fixture=args.fixture_binary.resolve())
    if any(not path.is_file() or not os.access(path, os.X_OK) for path in binaries.values()):
        parser.error("every frozen executable must exist; this script never builds missing inputs")
    before = {name: digest(path) for name, path in binaries.items()}
    args.out.mkdir(parents=True, exist_ok=True)
    if (args.out / "report.json").exists():
        parser.error("use a new output directory rather than overwriting existing evidence")
    os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(binaries["fixture"])
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-interaction-ui-") as temporary:
        harness = Interactions(Path(temporary), args.bin_dir.resolve(),
                               binaries["decoder"], args.out)
        try:
            report = harness.run()
        except Exception as error:
            harness.diagnostics()
            detail = f"{type(error).__name__}: {error}"
            for secret in harness.sensitive:
                if secret:
                    detail = detail.replace(secret, "[redacted]")
            raise SystemExit(detail) from None
        finally:
            harness.cleanup()
        after = {name: digest(path) for name, path in binaries.items()}
        if before != after:
            raise AssertionError("a frozen executable changed during the real PTY test")
        source_files = [
            "e2e_interactions.py", "e2e_input_history.py", "e2e_composer.py",
            "e2e_dashboard.py", "e2e_client_ui.py", "e2e_account_gateway.py", "e2e.py",
        ]
        report.update(source_revision=args.source_revision,
                      elapsed_seconds=round(time.monotonic() - started, 3),
                      binaries_sha256_before=before, binaries_sha256_after=after,
                      source_sha256={name: digest(Path(__file__).with_name(name))
                                     for name in source_files},
                      inputs_unchanged=True)
        encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        if any(secret and secret in encoded for secret in harness.sensitive):
            raise AssertionError("the shareable report contains a private fixture secret")
        (args.out / "report.json").write_text(encoded)
        print(json.dumps({"status": report["status"], "checks": report["checks"],
                          "elapsed_seconds": report["elapsed_seconds"],
                          "frames": len(report["frames"]),
                          "quiet_checks": report["quiet_checks"]}, ensure_ascii=False))


if __name__ == "__main__":
    main()
