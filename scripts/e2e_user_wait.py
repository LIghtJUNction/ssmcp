#!/usr/bin/env python3
"""Exercise authenticated user waits with a private account router and real PTY.

Consumes already-built CLI, library-fixture and VT-decoder binaries. Uses only
synthetic user input and notify:false; never invokes Cargo, a model or the
user's active terminal/account.
"""

import argparse
from concurrent.futures import ThreadPoolExecutor
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tempfile
import time

from e2e_input_history import InputHistory, ENTER, HOME, DOWN, PASTE_START, PASTE_END, text


class UserWait(InputHistory):
    def __init__(self, *arguments):
        super().__init__(*arguments)
        # The protocol permits a 25-second wait. HTTP must cover that explicit
        # budget and final response cleanup; it is not a retry timeout.
        self.http_timeout = 35
        self.workers = ThreadPoolExecutor(max_workers=4)
        self.wait_futures = []

    def state(self):
        return self.observe("ssmcp.ui.wait", {"op": "status"})["state"]

    def native_finish(self, wait_id, outcome):
        return self.observe("ssmcp.ui.wait", {
            "op": "finish", "wait_id": wait_id, "outcome": outcome,
        })

    def draft(self, front, value):
        self.focus_input(front)
        before = self.ui("/display/conversation")["draft"]
        self.check(before["cursor"] == len(before["text"].encode()),
                   "only the owned synthetic draft is replaced at its exact end cursor")
        front.send(b"\x7f" * (len(before["text"]) + 1)
                   + PASTE_START + value.encode() + PASTE_END)
        state = self.wait_state(front, lambda state: state["draft"]["text"] == value,
                                "the exact synthetic draft did not reach the composer")
        self.check(state["draft"]["cursor"] == len(value.encode()),
                   "draft UTF-8 and multiline cursor positions are preserved")
        return state

    def select_recipient(self, front, member=None):
        self.draft(front, "")
        self.agents(front)
        roster = self.queue()["agents"]
        # A successful join is authoritative on the server; the terminal sees
        # it after its next asynchronous steering snapshot. Observe only the
        # exact joined identity and never substitute another recipient.
        deadline = time.monotonic() + 8
        while member is not None and not any(
            entry["agent_id"] == member["agent_id"] for entry in roster
        ):
            if time.monotonic() >= deadline:
                raise AssertionError("the exact joined agent did not reach the terminal roster")
            front.pump(0.05)
            roster = self.queue()["agents"]
        index = 0 if member is None else 1 + next(
            index for index, entry in enumerate(roster)
            if entry["agent_id"] == member["agent_id"]
        )
        front.send(HOME + DOWN * index + ENTER)
        expected = None if member is None else member["agent_id"]
        self.wait_state(front, lambda state: state["target"] == expected,
                        "the owned fixture did not select its exact recipient")

    def begin(self, *, reason, intent="need_user", timeout_ms=20000, member=None):
        client_id = self.client_id
        arguments = {"timeout_ms": timeout_ms, "intent": intent, "reason": reason, "notify": False}
        code = "return await ui.wait(%s, %s);" % (json.dumps(client_id), json.dumps(arguments))
        if member is not None:
            code = "await collab.bind(%s, %s); %s" % (
                json.dumps(client_id), json.dumps({
                    "agent_id": member["agent_id"], "token": member["token"],
                }), code,
            )

        def request():
            status, envelope, _ = self.rpc("tools/call", {
                "name": "code_mode", "arguments": {"code": code, "timeout_ms": 30000},
            }, modern=True)
            if status != 200 or not isinstance(envelope, dict) or "error" in envelope:
                raise AssertionError(f"user wait HTTP response is invalid: status {status}")
            result = envelope["result"]
            if result.get("isError"):
                raise AssertionError(f"user wait orchestration failed: {result}")
            return result

        future = self.workers.submit(request)
        self.wait_futures.append(future)
        return future

    def active(self, front, count):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.03)
            state = self.state()
            if len(state["active"]) == count:
                return state
        raise AssertionError(f"expected {count} active native waits\n" + front.failure_detail())

    def completed(self, front, future, expected, *, wait_id=None):
        deadline = time.monotonic() + 8
        while not future.done() and time.monotonic() < deadline:
            front.pump(0.03)
        if not future.done():
            raise AssertionError("the completed native wait did not finish its HTTP response")
        response = future.result()
        receipts = response.get("structuredContent", {}).get("user_wait", [])
        self.check(len(receipts) == 1 and receipts[0]["status"] == expected,
                   f"the final HTTP receipt reports exactly one {expected} wait")
        receipt = receipts[0]
        self.check(receipt["client_id"] == self.client_id,
                   "the final receipt belongs to the exact active connection")
        if expected == "received":
            delivered = [(item["message_id"], item["revision"]) for item in self.messages(response)]
            referenced = [(item["message_id"], item["revision"]) for item in receipt["messages"]]
            self.check(bool(delivered) and referenced == delivered,
                       "receipt and user_messages match every exact message ID and revision")
        if wait_id is not None:
            self.check(receipt["wait_id"] == wait_id,
                       "the outer receipt retains the exact native wait identity")
        self.check(receipt["user_consent_to_end"] is None,
                   "a wait outcome does not manufacture the user's consent to end")
        if expected != "received":
            self.check(not self.messages(response) and not receipt["messages"],
                       "an unfinished delivery includes no ordinary user requirement")
        return response, receipt

    def targeted_unchanged(self, record):
        current = self.record(self.queue(), message_id=record["id"])
        self.check(current["text"] == record["text"] and current["status"] == "queued"
                   and current["target"] == record["target"],
                   "unbound wait/status/cancel leave the exact targeted requirement queued")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        subprocess.run(["/usr/bin/git", "-c", "init.defaultBranch=main", "init", "-q"],
                       cwd=self.workspace,
                       env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5)
        (self.workspace / "资料").mkdir()
        (self.workspace / "资料/等待 界.txt").write_text("SYNTHETIC_USER_WAIT_REFERENCE\n")
        front = self.start_front("user-wait")
        front.resize(28, 120)
        front.until(lambda shot: shot["size"] == [28, 120], "the wide user-wait terminal")
        member = self.public_call("collab", {
            "action": "join", "client_id": self.client_id,
            "name": "Exact wait recipient", "role": "isolated routing fixture",
        })
        self.sensitive.append(member["token"])
        self.select_recipient(front, member)
        ordinary = self.submit_source(front, "This requirement is only for the exact synthetic agent.")
        self.select_recipient(front)
        unsent = 'Unsubmitted draft @"资料/等待 界.txt"\n    keep indentation'
        self.draft(front, unsent)

        waits = [
            self.begin(reason="Synthetic input A"),
            self.begin(reason="Synthetic finish confirmation", intent="finish"),
            self.begin(reason="Synthetic input C"),
        ]
        active = self.active(front, 3)["active"]
        self.check(len({entry["wait_id"] for entry in active}) == 3
                   and len({entry["episode_id"] for entry in active}) == 1,
                   "three exact server wait IDs aggregate under one stable logical episode")
        self.check(all(not entry["notification_requested"] and entry["notification"] is None
                       for entry in active),
                   "this synthetic PTY run requests no real OS notification")
        self.check(self.ui("/display/conversation")["draft"]["text"] == unsent,
                   "starting concurrent waits preserves the complete unsent reference draft")
        front.until(lambda shot: "等待你确认是否结束" in text(shot),
                    "mixed-episode finish intent remains visible in the footer")
        front.until(lambda shot: "Click or type to wake" in text(shot),
                    "the authenticated user wait dims the production terminal")
        self.check(self.ui("/display")["sleeping"],
                   "the real terminal exposes its active visual sleep state")
        self.save_shot("user-wait-active-120", front, cells=True)
        front.resize(24, 48)
        front.until(lambda shot: shot["size"] == [24, 48]
                    and "Click or type to wake" in text(shot),
                    "the same pending wait renders a narrow sleep overlay")
        self.save_shot("user-wait-sleep-48", front, cells=True)
        front.resize(28, 120)
        front.until(lambda shot: shot["size"] == [28, 120]
                    and "Click or type to wake" in text(shot),
                    "the pending wait survives a wide resize")
        before_wake = self.ui("/display/conversation")
        # Fragment SGR press at its final byte, then drag and release. The
        # complete gesture wakes the overlay without reaching the conversation.
        front.send(b"\x1b[<0;3;3")
        front.pump(0.02)
        front.send(b"M\x1b[<32;5;4M\x1b[<0;5;4m")
        front.until(lambda shot: "Click or type to wake" not in text(shot),
                    "the owned mouse gesture removes the visual sleep overlay")
        self.check(not self.ui("/display")["sleeping"],
                   "mouse down and up leave the terminal visually awake")
        after_wake = self.ui("/display/conversation")
        self.check(all(after_wake[key] == before_wake[key]
                       for key in ("draft", "target", "view", "focus", "editing")),
                   "waking preserves the exact draft, recipient, view and focus")
        self.check(all(not future.done() for future in waits),
                   "mouse waking does not complete any authenticated wait")
        self.check({entry["wait_id"] for entry in self.state()["active"]}
                   == {entry["wait_id"] for entry in active},
                   "mouse waking leaves every native wait identity pending")
        self.targeted_unchanged(ordinary)
        self.save_shot("user-wait-mouse-awake-120", front, cells=True)
        # code_mode itself has four global workers. Keep three real external
        # waits, then exercise native slots four/five within the one remaining
        # worker. We do not confuse global code_mode busy with a wait receipt.
        capacity = self.response("""
            const client = %s, waitId = 'wait_capacity_fourth';
            const pending = call(client, 'ssmcp.ui.wait', {
                op:'wait', wait_id:waitId, episode_id:%s, recipient:'next',
                intent:'need_user', reason:'Synthetic native capacity', timeout_ms:20000, notify:false,
            });
            const result = {};
            try {
                for (let i=0;i<8;i++) {
                    const state = await call(client, 'ssmcp.ui.wait', {op:'status'});
                    result.state = state.structuredContent.state;
                    if (result.state.active.some(x=>x.wait_id===waitId)) break;
                }
                if (!result.state.active.some(x=>x.wait_id===waitId)) throw Error('fourth wait not active');
                const fifth = await call(client, 'ssmcp.ui.wait', {
                    op:'wait', wait_id:'wait_capacity_fifth', episode_id:%s, recipient:'next',
                    intent:'need_user', timeout_ms:20000, notify:false,
                });
                result.fifth = fifth.structuredContent;
            } finally {
                const finished = await call(client, 'ssmcp.ui.wait', {op:'finish',wait_id:waitId,outcome:'cancelled'});
                result.finish = finished.structuredContent;
                result.fourth = (await pending).structuredContent;
            }
            return result;
        """ % (json.dumps(self.client_id), json.dumps(active[0]["episode_id"]),
                 json.dumps(active[0]["episode_id"])))
        self.check(not self.messages(capacity),
                   "native capacity/status/finish calls never collect an ordinary requirement")
        capacity = capacity["structuredContent"]["result"]
        self.check(len(capacity["state"]["active"]) == 4
                   and capacity["fifth"]["wait_id"] == "wait_capacity_fifth"
                   and capacity["fifth"]["status"] == "busy",
                   "four native wait slots are exact and a fifth native request is explicitly busy")
        self.check(capacity["finish"]["status"] == "cancelled"
                   and capacity["fourth"]["status"] == "cancelled"
                   and len(self.state()["active"]) == 3,
                   "cleaning up the fourth native wait preserves all three server waits")
        self.targeted_unchanged(ordinary)

        self.draft(front, "/wait cancel wait_")
        front.send(ENTER)
        front.until(lambda shot: "等待编号前缀不唯一" in text(shot),
                    "ambiguous cancellation is explicitly rejected")
        self.check(len(self.state()["active"]) == 3
                   and self.ui("/display/conversation")["draft"]["text"] == "/wait cancel wait_",
                   "an ambiguous cancel preserves every active wait and the command draft")

        self.draft(front, "/wait")
        front.send(ENTER)
        front.until(lambda shot: "未请求系统通知" in text(shot), "real wait details in conversation")
        self.check(self.ui("/display/conversation")["draft"]["text"] == "/wait",
                   "inspection neither queues nor erases the local command")
        self.save_shot("user-wait-inspect-120", front, cells=True)
        by_reason = {entry["reason"]: entry for entry in active}
        first_id = by_reason["Synthetic input A"]["wait_id"]
        prefix = next(first_id[:length] for length in range(6, len(first_id) + 1)
                      if sum(entry["wait_id"].startswith(first_id[:length]) for entry in active) == 1)
        command = "/wait cancel " + prefix
        self.draft(front, command)
        front.send(ENTER)
        self.completed(front, waits[0], "cancelled", wait_id=first_id)
        self.check(len(self.state()["active"]) == 2
                   and self.ui("/display/conversation")["draft"]["text"] == command,
                   "unique-prefix terminal cancellation wakes only its exact wait and keeps the draft")
        finish = self.native_finish(first_id, "ready")
        self.check(not finish["finished"] and finish["status"] == "cancelled",
                   "a late native finish cannot overwrite the local cancellation")
        for future, reason in zip(waits[1:], ["Synthetic finish confirmation", "Synthetic input C"]):
            wait_id = by_reason[reason]["wait_id"]
            self.native_finish(wait_id, "cancelled")
            self.completed(front, future, "cancelled", wait_id=wait_id)
        self.active(front, 0)
        self.targeted_unchanged(ordinary)

        self.draft(front, "")
        delivered = self.begin(reason="Synthetic actual Enter delivery")
        delivery_id = self.active(front, 1)["active"][0]["wait_id"]
        body = "The real Enter key submits exactly this authenticated requirement."
        self.draft(front, body)
        front.send(ENTER)
        response, receipt = self.completed(front, delivered, "received", wait_id=delivery_id)
        messages = self.messages(response)
        self.check(len(messages) == 1 and messages[0]["text"] == body
                   and messages[0].get("agent_id") is None,
                   "the real keyboard requirement is the only next-caller delivery")
        self.check(len(receipt["messages"]) == 1
                   and receipt["messages"][0]["message_id"] == messages[0]["message_id"],
                   "the final wait receipt and user_messages share the exact message identity")
        self.wait_state(front, lambda state: not state["draft"]["text"],
                        "the acknowledged Enter draft did not clear")
        # The HTTP response commits delivery before the client receives its
        # asynchronous queue snapshot. Observe that exact revision; do not
        # infer delivery from a different request or resubmit anything.
        deadline = time.monotonic() + 8
        current = self.record(self.queue(), message_id=messages[0]["message_id"])
        while not (current["status"] == "delivered"
                   and current["revision"] == messages[0]["revision"]) and time.monotonic() < deadline:
            front.pump(0.05)
            current = self.record(self.queue(), message_id=messages[0]["message_id"])
        self.check(current["status"] == "delivered"
                   and current["revision"] == messages[0]["revision"],
                   "the queue confirms the exact committed message ID and revision")
        self.targeted_unchanged(ordinary)

        self.draft(front, unsent)
        timed = self.begin(reason="Synthetic deadline", timeout_ms=1000)
        response, receipt = self.completed(front, timed, "timed_out")
        self.check(receipt["continue_wait"] is True
                   and isinstance(response["structuredContent"].get("continuation_reminder", {}).get("instruction"), str),
                   "timeout carries the explicit continue-wait instruction")
        self.active(front, 0)
        self.check(self.ui("/display/conversation")["draft"]["text"] == unsent,
                   "a local/server deadline never submits or clears the unsent draft")
        front.resize(24, 48)
        front.until(lambda shot: shot["size"] == [24, 48], "the narrow wait terminal")
        self.draft(front, "/wait")
        front.send(ENTER)
        # Narrow rows may wrap the precise ID and its status across lines.
        # Ignore screen spacing, while requiring both the same ID and outcome.
        timeout_detail = "#" + receipt["wait_id"] + "·本轮等待超时"
        front.until(lambda shot: timeout_detail in "".join(text(shot).split()),
                    "the exact completed wait and its wrapped timeout status in /wait details")
        self.draft(front, unsent)
        self.save_shot("user-wait-timeout-48", front, cells=True)

        forged = self.begin(reason="Synthetic premature native ready")
        forged_id = self.active(front, 1)["active"][0]["wait_id"]
        self.native_finish(forged_id, "ready")
        self.completed(front, forged, "cancelled", wait_id=forged_id)
        self.targeted_unchanged(ordinary)
        exact = self.begin(reason="Synthetic exact-agent receipt", member=member)
        response, receipt = self.completed(front, exact, "received")
        self.expect_delivery(response, ordinary, agent_id=member["agent_id"])
        self.check(receipt["recipient"] == member["agent_id"],
                   "only the authenticated exact-agent wait receives its targeted requirement")
        again = self.begin(reason="Synthetic no-replay deadline", member=member, timeout_ms=1000)
        self.completed(front, again, "timed_out")

        disconnected = self.begin(reason="Synthetic WSS disconnect")
        disconnect_id = self.active(front, 1)["active"][0]["wait_id"]
        previous_id = self.client_id
        self.account_gateway.disconnect_client(previous_id)
        self.completed(front, disconnected, "disconnected", wait_id=disconnect_id)
        credentials = self.new_credentials(front)
        self.client_id = credentials["client_id"]
        front.client_id = self.client_id
        front.credentials = credentials
        self.check(self.client_id != previous_id and not self.state()["active"]
                   and not self.state()["recent"],
                   "a replacement connection inherits no active waits or old completion ledger")
        self.check(self.ui("/display/conversation")["draft"]["text"] == unsent,
                   "WSS disconnect/reconnect preserves the exact unsubmitted reference draft")
        self.save_shot("user-wait-reconnected-48", front, cells=True)
        for secret in self.sensitive:
            self.check(secret.encode() not in front.transcript,
                       "automatic user-wait terminal output contains no owned credential")
        # The integrated composer clears a draft, then asks for a second
        # empty-input Ctrl-C. Follow that contract before checking shutdown.
        self.focus_input(front)
        front.resize(28, 120)
        front.until(lambda shot: shot["size"] == [28, 120], "the exit confirmation viewport")
        queued = self.queue()["queue"]
        front.send(b"\x03")
        self.wait_state(front, lambda state: not state["draft"]["text"],
                        "Ctrl-C clears only the owned draft before exit")
        self.check(front.process.poll() is None and self.queue()["queue"] == queued,
                   "clearing the user-wait draft leaves the client and ordinary queue intact")
        front.send(b"\x03")
        front.until(lambda shot: "Press Ctrl+C again to exit" in text(shot),
                    "the explicit exit confirmation")
        self.check(front.process.poll() is None,
                   "empty-input Ctrl-C warns before the user-wait client exits")
        final, restored = front.disconnect()
        self.check(not final["alternate_screen"] and restored == front.original_termios,
                   "the isolated user-wait terminal restores its own termios")

    def close_waits(self):
        try:
            for active in self.state()["active"]:
                self.native_finish(active["wait_id"], "cancelled")
        except Exception:
            # Each request also has its explicit 25s native/server deadline and
            # the owned HTTP socket's 35s ceiling. Never touch another client.
            pass
        self.workers.shutdown(wait=True, cancel_futures=True)


def file_sha(path):
    with Path(path).open("rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    report = {
        "status": "failed",
        "transport": "production account router over loopback HTTP/WebSocket; synthetic GitHub provider",
        "scope": "real CLI/PTY and authenticated waits; notify:false; no model invocation",
        "binary_sha256": file_sha(args.bin_dir / "ssmcp"),
        "fixture_sha256": file_sha(os.environ["SSMCP_E2E_FIXTURE_BINARY"]),
        "decoder_sha256": file_sha(args.decoder),
        "script_sha256": file_sha(__file__),
    }
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-user-wait-e2e-") as directory:
        harness = UserWait(Path(directory), args.bin_dir, args.decoder, args.out)
        try:
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            message = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    message = message.replace(secret, "[redacted]")
            report["error"] = message
            if harness.last_frontend and not harness.last_frontend.closed:
                harness.save_shot("user-wait-failure", harness.last_frontend, cells=True)
            harness.diagnostics()
        finally:
            harness.close_waits()
            harness.close_frontends()
            harness.cleanup()
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            (args.out / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "error": report.get("error")}, ensure_ascii=False))
    raise SystemExit(report["status"] != "passed")


if __name__ == "__main__":
    main()
