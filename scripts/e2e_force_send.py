#!/usr/bin/env python3
"""Check explicit listener receipts through a private account router and real PTY.

Consumes already-built client/library-fixture/VT-decoder binaries. This script
never invokes Cargo, an installed model, or the user's active connection.
"""

import argparse
import hashlib
import json
import os
import signal
from pathlib import Path
import subprocess
import tempfile
import time

from e2e_input_history import (
    InputHistory, ENTER, HOME, PASTE_START, PASTE_END,
    REFERENCE_BEGIN, REFERENCE_END, text,
)


class ForceSend(InputHistory):
    def listener(self, op, **arguments):
        return self.observe("ssmcp.ui.listener", {"op": op, **arguments})

    def listener_error(self, op, **arguments):
        code = "return await call(%s, 'ssmcp.ui.listener', %s);" % (
            json.dumps(self.client_id), json.dumps({"op": op, **arguments}),
        )
        status, envelope, _ = self.rpc(
            "tools/call", {"name": "code_mode", "arguments": {"code": code}}, modern=True,
        )
        self.check(status == 200 and isinstance(envelope, dict) and "error" not in envelope,
                   "an expected listener rejection remains a valid MCP tool response")
        result = envelope["result"]
        self.check(not self.messages(result),
                   "a rejected listener operation does not collect the ordinary queue")
        native = result if result.get("isError") else result["structuredContent"]["result"]
        self.check(native.get("isError") is True,
                   "invalid listener operations return a real native error")
        return " ".join(item.get("text", "") for item in native.get("content", [])
                        if item.get("type") == "text")

    def open_listener(self):
        lease = self.listener("open", label="Synthetic PTY receiver")
        self.sensitive.append(lease["token"])
        self.check(lease["schema_version"] == 1 and lease["connection_id"] == self.client_id
                   and len(lease["listener_id"]) == 32 and len(lease["token"]) == 64,
                   "explicit open binds an exclusive credential to this exact connection")
        self.check(lease["lease_ms"] == 30000 and 29000 <= lease["expires_in_ms"] <= 30000 and lease["ack_timeout_ms"] == 5000,
                   "the advertised lease and ACK budgets match the protocol")
        return {"listener_id": lease["listener_id"], "token": lease["token"]}

    def draft(self, front, value):
        self.focus_input(front)
        before = self.ui("/display/conversation")["draft"]
        self.check(before["cursor"] == len(before["text"].encode()),
                   "the fixture changes only its own draft at the exact end cursor")
        # Backspace crosses logical line boundaries. Bracketed paste preserves
        # indentation and newlines without accidentally pressing Enter.
        front.send(b"\x7f" * (len(before["text"]) + 1)
                   + PASTE_START + value.encode() + PASTE_END)
        state = self.wait_state(front, lambda current: current["draft"]["text"] == value,
                                "the exact synthetic draft was not entered")
        self.check(state["draft"]["cursor"] == len(value.encode()),
                   "bracketed input preserves the exact UTF-8 body and cursor")
        return state

    def disconnect_front(self, front):
        """Follow the current clear/remind/exit contract on this owned PTY."""
        self.focus_input(front)
        front.resize(28, 120)
        front.until(lambda shot: shot["size"] == [28, 120], "the exit confirmation viewport")
        queued = self.queue()["queue"]
        if self.ui("/display/conversation")["draft"]["text"]:
            front.send(b"\x03")
            self.wait_state(front, lambda state: not state["draft"]["text"],
                            "Ctrl-C clears only the owned draft before exit")
            self.check(front.process.poll() is None and self.queue()["queue"] == queued,
                       "clearing the draft keeps the client open and preserves its ordinary queue")
        front.send(b"\x03")
        front.until(lambda shot: "Press Ctrl+C again to exit" in text(shot),
                    "the explicit exit confirmation")
        self.check(front.process.poll() is None,
                   "the empty-input Ctrl-C warns without closing this owned client")
        return front.disconnect()

    def force(self, front, value):
        self.draft(front, value)
        front.send(ENTER)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.03)
            status = self.listener("status")
            if status["pending"]:
                self.check(self.ui("/display/conversation")["draft"]["text"] == value,
                           "a pending force-send preserves the original draft")
                return status["pending"]["delivery_id"]
        raise AssertionError("force-send did not create an in-flight delivery\n" + front.failure_detail())

    def receipt(self, front, delivery_id, expected):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.03)
            status = self.listener("status")
            receipts = [item for item in status["receipts"] if item["delivery_id"] == delivery_id]
            if receipts and not self.ui("/display/conversation")["force_pending"]:
                self.check(len(receipts) == 1 and receipts[0]["status"] == expected,
                           f"delivery has one authoritative {expected} receipt")
                self.check(status["pending"] is None,
                           "a final receipt releases the one in-flight message slot")
                return receipts[0]
        raise AssertionError(f"the {expected} receipt did not reach the UI\n" + front.failure_detail())

    def ordinary_unchanged(self, ordinary):
        queue = self.queue()["queue"]
        self.check(len(queue) == 1 and queue[0]["id"] == ordinary["id"]
                   and queue[0]["text"] == ordinary["text"] and queue[0]["status"] == "queued",
                   "listener calls neither consume nor add to the exact ordinary queued request")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        subprocess.run(["/usr/bin/git", "-c", "init.defaultBranch=main", "init", "-q"],
                       cwd=self.workspace,
                       env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5)
        path = "资料/说明 界.txt"
        source_file = "SYNTHETIC_FORCE_REFERENCE\n中文材料\n"
        (self.workspace / "资料").mkdir()
        (self.workspace / path).write_text(source_file, encoding="utf-8")
        front = self.start_front("force-send")
        front.resize(28, 120)
        front.until(lambda shot: shot["size"] == [28, 120], "the wide force-send fixture")
        roster = self.public_call("messages", {
            "action": "list", "client_id": self.client_id,
        })
        inboxes = [entry for entry in roster["recipients"]
                   if entry["client_id"] == self.client_id
                   and entry["kind"] == "account_inbox" and entry["agent_id"] is None]
        self.check(len(inboxes) == 1 and inboxes[0]["online"] and inboxes[0]["sendable"],
                   "the server lists one exact available account inbox for this owned client")
        native_recipient = inboxes[0]["id"]
        self.check(native_recipient == "client:" + self.client_id,
                   "the native routing guard uses an authoritative stable recipient ID")
        ordinary = self.submit_source(front, "Ordinary request stays in its own queue")

        original = '  /send @"' + path + '"\n    code\n'
        self.draft(front, original)
        front.send(ENTER)
        front.until(lambda shot: "发送失败：没有程序在监听" in text(shot),
                    "an explicit no-listener failure for a leading-space command")
        self.check(self.ui("/display/conversation")["draft"]["text"] == original,
                   "no listener preserves leading spaces, file reference, indentation and final newline")
        self.check(self.listener("status")["listener"] is None,
                   "ordinary tool observations do not register a listener")
        self.ordinary_unchanged(ordinary)
        self.save_shot("force-no-listener-120", front, cells=True)

        self.draft(front, "/send   ")
        front.send(ENTER)
        front.until(lambda shot: "用法：/send" in text(shot), "feedback for an empty force-send body")
        self.check(self.ui("/display/conversation")["draft"]["text"] == "/send   ",
                   "empty force-send does not disappear silently or enter the ordinary queue")

        lease = self.open_listener()
        self.check("listener_busy" in self.listener_error("open", label="Second receiver"),
                   "a second listener cannot replace the first live lease")
        snapshot = self.listener("status")
        self.check(lease["token"] not in json.dumps(snapshot),
                   "listener status contains no lease credential")

        missing_body = "@missing.txt\n"
        missing_original = "/send " + missing_body
        delivery_id = self.force(front, missing_original)
        offered = self.listener("read", **lease, wait_ms=0)["delivery"]
        repeated = self.listener("read", **lease, wait_ms=0)["delivery"]
        self.check(offered == repeated and offered["delivery_id"] == delivery_id
                   and offered["connection_id"] == self.client_id
                   and offered["listener_id"] == lease["listener_id"]
                   and offered["source_text"] == missing_body and offered["source"] == "client_user",
                   "missing-reference delivery retains its exact listener, identity and original body")
        raw, separator, encoded = offered["text"].partition(REFERENCE_BEGIN)
        self.check(raw == missing_body and bool(separator) and encoded.endswith(REFERENCE_END),
                   "missing-reference force-send reaches the receiver with an explicit reference payload")
        reference = json.loads(encoded[:-len(REFERENCE_END)])
        entries = reference["references"]
        self.check(reference["schema_version"] == 1
                   and reference["source_sha256"] == hashlib.sha256(missing_body.encode()).hexdigest()
                   and len(entries) == 1 and entries[0]["kind"] == "file"
                   and entries[0]["path"] == "missing.txt" and entries[0]["status"] == "unavailable"
                   and bool(entries[0].get("warning"))
                   and "read" not in entries[0] and "preview" not in entries[0],
                   "the receiver gets an unavailable-file warning without fabricated contents")
        pending = front.until(lambda shot: "等待接收确认" in text(shot),
                              "the missing reference still waiting for the real listener ACK")
        self.check("引用提示：" not in text(pending)
                   and self.ui("/display/conversation")["draft"]["text"] == missing_original,
                   "a missing-reference notice and draft clearing wait for an accepted ACK")
        self.listener("ack", **lease, delivery_id=delivery_id, status="accepted")
        self.receipt(front, delivery_id, "accepted")
        self.wait_state(front, lambda state: not state["draft"]["text"],
                        "the accepted missing-reference draft did not clear")
        front.until(lambda shot: "监听程序已接收" in text(shot)
                    and any("引用提示：" in row and "@missing.txt" in row for row in shot["rows"]),
                    "the missing-reference notice rendered only after the accepted receipt")
        self.check(not (self.workspace / "missing.txt").exists(),
                   "accepting an unavailable reference does not create a file")
        self.ordinary_unchanged(ordinary)
        self.save_shot("force-missing-accepted-120", front, cells=True)

        body = '@"' + path + '"\n    code\n'
        original = "/send\n" + body
        delivery_id = self.force(front, original)
        first = self.listener("read", **lease, wait_ms=0)["delivery"]
        repeated = self.listener("read", **lease, wait_ms=0)["delivery"]
        self.check(first == repeated and first["delivery_id"] == delivery_id,
                   "read retries return the same complete delivery identity and body")
        self.check(first["source_text"] == body and first["source"] == "client_user",
                   "a newline command delimiter preserves the exact original force-send body")
        raw, separator, references = first["text"].partition(REFERENCE_BEGIN)
        self.check(raw == body and bool(separator) and references.endswith(REFERENCE_END),
                   "forced delivery expands references without rewriting the source text")
        reference = json.loads(references[:-len(REFERENCE_END)])
        self.check(reference["source_sha256"] == hashlib.sha256(body.encode()).hexdigest()
                   and len(reference["references"]) == 1
                   and reference["references"][0]["path"] == path
                   and reference["references"][0]["preview"] == source_file,
                   "the listener receives the selected Unicode/space file with an exact source digest")
        accepted = self.listener("ack", **lease, delivery_id=delivery_id, status="accepted")
        self.check(self.listener("ack", **lease, delivery_id=delivery_id, status="accepted") == accepted,
                   "a duplicate identical ACK returns the original receipt")
        self.receipt(front, delivery_id, "accepted")
        self.wait_state(front, lambda state: not state["draft"]["text"],
                        "an unchanged accepted draft was not cleared")
        front.until(lambda shot: "监听程序已接收" in text(shot), "the precise listener receipt wording")
        self.save_shot("force-accepted-120", front, cells=True)
        self.check("ack_conflict" in self.listener_error(
            "ack", **lease, delivery_id=delivery_id, status="rejected", reason="changed outcome"),
            "a conflicting ACK cannot rewrite the terminal receipt")
        self.ordinary_unchanged(ordinary)

        original = "/send Identical text typed again"
        delivery_id = self.force(front, original)
        self.listener("read", **lease, wait_ms=0)
        # The final text is identical, but real keyboard editing creates a new
        # revision. An older receipt must not clear that newly edited draft.
        self.draft(front, original)
        self.listener("ack", **lease, delivery_id=delivery_id, status="accepted")
        self.receipt(front, delivery_id, "accepted")
        self.check(self.ui("/display/conversation")["draft"]["text"] == original,
                   "late ACK preserves an identical body entered as a new draft revision")

        original = '/send @"' + path + '"\n'
        delivery_id = self.force(front, original)
        self.listener("read", **lease, wait_ms=0)
        self.listener("ack", **lease, delivery_id=delivery_id,
                      status="rejected", reason="Synthetic receiver declines this message")
        self.receipt(front, delivery_id, "rejected")
        self.check(self.ui("/display/conversation")["draft"]["text"] == original,
                   "explicit rejection preserves the complete attachment/reference draft")

        original = "/send Offered without an ACK"
        delivery_id = self.force(front, original)
        self.listener("read", **lease, wait_ms=0)
        self.receipt(front, delivery_id, "unknown")
        self.check(self.ui("/display/conversation")["draft"]["text"] == original,
                   "an offered but unconfirmed message keeps the user's draft")
        self.check(self.listener("read", **lease, wait_ms=0)["delivery"] is None,
                   "unknown delivery is not automatically replayed to the listener")
        self.check("ack_conflict" in self.listener_error(
            "ack", **lease, delivery_id=delivery_id, status="accepted"),
            "a late ACK cannot turn unknown into success")
        self.ordinary_unchanged(ordinary)
        front.resize(24, 48)
        front.until(lambda shot: shot["size"] == [24, 48], "the narrow force-send fixture")
        self.save_shot("force-unknown-48", front, cells=True)

        # Account relay recipients now come from native messaging, not the
        # legacy steering roster. Exercise that real picker here; the existing
        # Rust exact-agent/thread tests retain the legacy listener-scope guard.
        front.resize(28, 120)
        front.until(lambda shot: shot["size"] == [28, 120], "the native recipient guard viewport")
        self.draft(front, "/message " + native_recipient)
        front.send(ENTER)
        self.wait_state(front, lambda state: state["view"] == "agents"
                        and state["selected_agent"] == "native:" + native_recipient,
                        "the real message command did not select the exact native inbox row")
        front.send(ENTER)
        selected = self.wait_state(
            front, lambda state: state["native_recipient"] == native_recipient
            and state["focus"] == "input",
            "the real picker did not retain the chosen native recipient",
        )
        inbox_before = self.public_call("messages", {
            "action": "inbox", "client_id": self.client_id,
        })
        self.check(inbox_before["messages"] == [] and not inbox_before.get("user_messages"),
                   "the owned native inbox starts empty without consuming the ordinary queue")
        listener_before = self.listener("status")
        original = "/send Do not change this recipient\n    @missing.txt $missing-skill\n"
        before = self.draft(front, original)
        front.send(ENTER)
        front.until(lambda shot: "此收件人不支持 /send" in text(shot),
                    "the explicit native-recipient force-send rejection")
        state = self.ui("/display/conversation")
        self.check(state["native_recipient"] == native_recipient
                   and state["target"] == selected["target"]
                   and state["draft"] == before["draft"],
                   "rejected force-send preserves the exact native recipient, UTF-8 draft and cursor")
        self.check(not state["native_pending"] and not state["force_pending"],
                   "rejected force-send starts neither a native send nor a listener task")
        listener_after = self.listener("status")
        self.check(listener_before["pending"] is None and listener_after["pending"] is None
                   and listener_after["receipts"] == listener_before["receipts"],
                   "the default listener receives no rerouted delivery or new receipt")
        inbox_after = self.public_call("messages", {
            "action": "inbox", "client_id": self.client_id,
        })
        self.check(inbox_after["messages"] == [] and not inbox_after.get("user_messages"),
                   "the native inbox receives zero ordinary messages from the rejected /send")
        self.ordinary_unchanged(ordinary)
        self.save_shot("force-native-recipient-rejected-120", front, cells=True)
        self.draft(front, "")
        self.agents(front)
        front.send(HOME + ENTER)
        self.wait_state(front, lambda state: state["target"] is None
                        and state["native_recipient"] is None,
                        "the fixture did not explicitly restore Next caller")
        self.listener("read", **lease, wait_ms=0)

        original = "/send Disconnect after read"
        delivery_id = self.force(front, original)
        self.listener("read", **lease, wait_ms=0)
        previous_id = self.client_id
        self.account_gateway.disconnect_client(previous_id)
        credentials = self.new_credentials(front)
        self.client_id = credentials["client_id"]
        front.client_id = self.client_id
        front.credentials = credentials
        self.check(self.client_id != previous_id,
                   "the owned WSS disconnect creates a new connection identity")
        front.until(lambda shot: "上次连接：发送结果未确认" in text(shot)
                    and "不会自动重发" in text(shot),
                    "the original sender's explicit unknown outcome after reconnection")
        self.check(not self.ui("/display/conversation")["force_pending"],
                   "the disconnected sender finishes without resubmitting its delivery")
        status = self.listener("status")
        self.check(status["listener"] is None and status["pending"] is None,
                   "reconnection restores neither the old lease nor its forced delivery")
        self.check(status["receipts"] == [] and delivery_id not in json.dumps(status),
                   "the new network listener status contains no previous connection receipt")
        self.check(self.ui("/listener")["receipts"] == [],
                   "the new UI listener projection contains no previous connection receipt")
        # Conversation snapshots contain generic headings, not message bodies.
        # Leave and re-enter the conversation to clear the transient notice,
        # then inspect the real retained card without editing the user's draft.
        draft_before_navigation = self.ui("/display/conversation")["draft"]
        self.activate_column(front, "queue")
        self.activate_column(front, "conversation")
        retained = front.until(
            lambda shot: "上次连接：发送结果未确认" in text(shot)
            and "不会自动重发" in text(shot),
            "the interrupted sender's retained card after clearing the transient notice",
        )
        self.check(text(retained).count("上次连接：发送结果未确认") == 1
                   and self.ui("/display/conversation")["draft"] == draft_before_navigation,
                   "the real conversation renders the interrupted outcome once and keeps its draft")
        self.save_shot("force-reconnected-unknown-120", front, cells=True)
        self.check(self.ui("/display/conversation")["draft"]["text"] == original,
                   "connection loss preserves the unconfirmed force-send draft")
        self.listener_error("read", **lease, wait_ms=0)
        replacement = self.open_listener()
        self.listener_error("ack", **lease, delivery_id=delivery_id, status="accepted")
        self.check(self.listener("read", **replacement, wait_ms=0)["delivery"] is None,
                   "a replacement listener receives no automatic replay from the old connection")
        self.listener("close", **replacement)
        self.check(all(record["id"] == ordinary["id"] for record in self.queue()["queue"]),
                   "none of the forced messages enter the ordinary queue after reconnect")
        for secret in self.sensitive:
            self.check(secret.encode() not in front.transcript,
                       "automatic terminal output does not expose an owned credential")
        final, restored = self.disconnect_front(front)
        self.check(not final["alternate_screen"] and restored == front.original_termios,
                   "the force-send fixture restores its own terminal exactly")


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("listener E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    args.out.mkdir(parents=True, exist_ok=True)
    report = {
        "status": "failed",
        "transport": "production account router over loopback HTTP/WebSocket; synthetic GitHub provider",
        "scope": "real CLI/PTY and explicit synthetic listener; no model invocation",
        "routing_scenario": "real native account inbox picker rejection; registered-thread scope remains covered by Rust regressions",
        "binary_sha256": hashlib.file_digest((args.bin_dir / "ssmcp").open("rb"), "sha256").hexdigest(),
        "script_sha256": hashlib.file_digest(Path(__file__).open("rb"), "sha256").hexdigest(),
    }
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-force-send-e2e-") as directory:
        harness = ForceSend(Path(directory), args.bin_dir, args.decoder, args.out)
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
                harness.save_shot("force-failure", harness.last_frontend, cells=True)
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
            owned = list(dict.fromkeys([*harness.processes, harness.account_gateway.process]))
            report["cleanup"] = {
                "owned_processes": [{"pid": item.pid, "exit_code": item.poll()} for item in owned],
                "all_exited": all(item.poll() is not None for item in owned),
            }
            if not report["cleanup"]["all_exited"]:
                report["status"] = "failed"
                report["error"] = "an owned process remained after cleanup"
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            (args.out / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "error": report.get("error")}, ensure_ascii=False))
    raise SystemExit(report["status"] != "passed")


if __name__ == "__main__":
    main()
