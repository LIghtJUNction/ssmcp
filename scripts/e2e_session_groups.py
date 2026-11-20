#!/usr/bin/env python3
"""Exercise exact session groups through owned PTYs and a real loopback relay.

Requires prebuilt ssmcp, ssmcpd, the account-router fixture and vt_snapshot.
No Cargo, external model, existing client, desktop, or production account is used.
Only the upstream OAuth identity provider is synthetic; routing, authentication,
PTY input, receipts, and source disconnects use the product implementations.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import subprocess
import tempfile
import time
import traceback

from e2e_composer import ENTER
from e2e_dashboard import text
from e2e_input_history import PASTE_END, PASTE_START
from e2e_interactions import Interactions, digest


GROUP_ID = re.compile(r"\bui-group-[0-9a-f]{32}\b")
RECEIPT_FIELDS = ("request_id", "message_id", "recipient_id", "created_at_ms")


def child_request_id(group_id, recipient_id):
    canonical = json.dumps([group_id, recipient_id], separators=(",", ":")).encode()
    return "group-" + hashlib.sha256(canonical).hexdigest()


class SessionGroups(Interactions):
    def __init__(self, directory, binary_dir, decoder, output):
        super().__init__(directory, binary_dir, decoder, output)
        self.fixture_timeout_seconds = 300
        self.group_evidence = []
        self.receipt_evidence = []
        self.disconnect_evidence = None

    def capture(self, front, name, marker):
        for columns in (120, 48):
            front.resize(32, columns)
            shot = front.until(
                lambda value: value["size"] == [32, columns] and marker in text(value),
                name + " at " + str(columns) + " columns", cells=True)
            stem = "session-groups-" + name + "-" + str(columns)
            encoded = json.dumps(shot, ensure_ascii=False, indent=2) + "\n"
            if any(secret and secret in encoded for secret in [self.secret, *self.sensitive]):
                raise AssertionError("private fixture credentials appeared in a PTY frame")
            (self.output / (stem + ".json")).write_text(encoded, encoding="utf-8")
            (self.output / (stem + ".txt")).write_text(text(shot) + "\n", encoding="utf-8")
            self.frames.append(stem + ".json")
        front.resize(32, 120)
        front.pump(0.15)

    def receive(self, recipient):
        result = self.response(
            "return await call(%s, 'terminal.status', {});" % json.dumps(recipient["client_id"]),
            agent=recipient.get("binding"))
        self.check(not self.messages(result),
                   "native group delivery does not create an ordinary steering requirement")
        native = result["structuredContent"]["result"]
        if native.get("isError"):
            raise AssertionError("the owned recipient's actual tool call failed")
        return result["structuredContent"].get("native_messages", [])

    def inbox(self, recipient):
        return self.messages_call(
            "inbox", recipient["client_id"], agent=recipient.get("binding"))["messages"]

    def acknowledge(self, recipient, receipt):
        reply = self.messages_call(
            "ack", recipient["client_id"], {"message_id": receipt["message_id"]},
            agent=recipient.get("binding"))
        receipts = reply.get("receipts", [])
        self.check(len(receipts) == 1, "the exact receiver returns one acknowledgement receipt")
        current = receipts[0]
        self.check(all(current[key] == receipt[key] for key in RECEIPT_FIELDS)
                   and current["status"] == "acknowledged",
                   "acknowledgement keeps the original request, message and recipient identities")
        return current

    def select(self, front, recipients):
        expected = set()
        for recipient in recipients:
            self.open_picker(front, recipient["id"])
            before = self.ui("/display/conversation")
            self.check(set(before["group_selected"]) == expected,
                       "highlighting a precise session row does not select or send it")
            self.check(before["selected_agent"] == "native:" + recipient["id"],
                       "the highlighted row names this exact remote process or thread")
            self.check(all(not self.inbox(item) for item in recipients),
                       "all target inboxes remain empty before explicit group submission")
            front.send(b" ")
            expected.add(recipient["id"])
            self.wait_state(
                front, lambda state: set(state["group_selected"]) == expected,
                "Space selecting only the highlighted exact recipient")
        self.check(len(expected) == 3, "the mixed group contains exactly three explicit recipients")
        self.capture(front, "selected", "Sessions")
        return expected

    def submit_group(self, front, body, selected, *, compose=False, previous=()):
        source = ("/group send " if compose else "/send ") + body
        if compose:
            front.send(b"g")
            self.wait_state(
                front, lambda state: state["view"] == "activity"
                and state["focus"] == "input" and state["draft"]["text"] == "/group send ",
                "g opens a group draft without sending")
            self.send_stream(front, PASTE_START + body.encode() + PASTE_END)
        else:
            self.replace_short(front, source)
        self.wait_state(
            front, lambda state: state["draft"]["text"] == source
            and set(state["group_selected"]) == selected,
            "the complete literal group draft and exact selection are visible before Enter")
        front.send(ENTER)
        self.wait_state(front, lambda state: state["draft"]["text"] == "",
                        "only individual service receipts clear the original group draft")
        shot = front.until(
            lambda value: len(set(GROUP_ID.findall(value["rows"][-1])) - set(previous)) == 1,
            "the service-confirmed group summary carries its immutable request ID")
        group_id = (set(GROUP_ID.findall(shot["rows"][-1])) - set(previous)).pop()
        self.check(set(self.ui("/display/conversation")["group_selected"]) == selected,
                   "a successful send preserves the user's reusable explicit recipient combination")
        evidence = {
            "group_request_id": group_id,
            "text": body,
            "text_sha256": hashlib.sha256(body.encode()).hexdigest(),
            "selected_recipient_ids": sorted(selected),
            "child_request_ids": {target: child_request_id(group_id, target)
                                  for target in sorted(selected)},
        }
        self.group_evidence.append(evidence)
        return evidence

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        subprocess.run(
            ["git", "-c", "init.defaultBranch=main", "init", "-q"], cwd=self.workspace,
            env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5)
        front = self.start_front("group-source")
        source_id = front.client_id

        first, first_credentials = self.start_client(
            "group-process-a", name="Group process A", cwd=self.workspace)
        second, second_credentials = self.start_client(
            "group-process-b", name="Group process B", cwd=self.workspace)
        a_id, b_id = first_credentials["client_id"], second_credentials["client_id"]
        self.check(first.poll() is None and second.poll() is None
                   and len({source_id, a_id, b_id}) == 3,
                   "one owned source PTY and two distinct owned recipient processes are live")
        member = self.public_call("collab", {
            "action": "join", "client_id": a_id,
            "name": "Exact group thread", "role": "owned deterministic receipt fixture"})
        self.sensitive.append(member["token"])
        binding = {"client_id": a_id, "id": member["agent_id"], "token": member["token"]}
        recipients = [
            {"id": "client:" + a_id, "client_id": a_id, "kind": "account_inbox"},
            {"id": "agent:" + a_id + ":" + member["agent_id"], "client_id": a_id,
             "kind": "agent", "binding": binding},
            {"id": "client:" + b_id, "client_id": b_id, "kind": "account_inbox"},
        ]
        directory = self.messages_call("list", source_id)["recipients"]
        for recipient in recipients:
            matches = [entry for entry in directory if entry["id"] == recipient["id"]]
            self.check(len(matches) == 1 and matches[0]["online"] and matches[0]["sendable"]
                       and matches[0]["kind"] == recipient["kind"]
                       and matches[0]["client_id"] == recipient["client_id"],
                       "the authenticated directory advertises the exact selected identity")
        selected = self.select(front, recipients)
        queue_before = self.queue()["queue"]
        body = ("  MIXED_GROUP_LITERAL 中文原文\n"
                "!touch .group-must-not-execute\n"
                "@./missing-group-file $not-a-skill  trailing spaces  \n")
        group = self.submit_group(front, body, selected, compose=True)
        self.check(self.queue()["queue"] == queue_before
                   and not (self.workspace / ".group-must-not-execute").exists(),
                   "group text keeps literal references and shell syntax outside the steering queue")
        self.capture(front, "queued", "MIXED_GROUP_LITERAL")

        received_ids = set()
        for index, recipient in enumerate(recipients):
            delivered = self.receive(recipient)
            self.check(len(delivered) == 1, "each exact target receives one native group message")
            message = delivered[0]
            receipt = message["receipt"]
            self.check(message["text"] == body
                       and message["recipient"]["id"] == recipient["id"]
                       and message["recipient"]["kind"] == recipient["kind"]
                       and message["sender"]["kind"] == "local_user"
                       and message["sender"]["client_id"] == source_id
                       and receipt["recipient_id"] == recipient["id"]
                       and receipt["request_id"] == group["child_request_ids"][recipient["id"]]
                       and receipt["status"] == "included"
                       and receipt["message_id"] not in received_ids,
                       "the actual tool response preserves exact text, routing, origin and child request ID")
            received_ids.add(receipt["message_id"])
            self.check(not self.receive(recipient),
                       "a second ordinary tool response never includes the same accepted message again")
            # This unbound A call happens before the exact A thread is consumed.
            if index == 0:
                self.check(not self.receive(recipient),
                           "the process inbox cannot take the pending exact-thread message on the same process")
            included = self.inbox(recipient)
            self.check(len(included) == 1 and included[0]["text"] == body
                       and all(included[0]["receipt"][key] == receipt[key] for key in RECEIPT_FIELDS),
                       "explicit inbox review returns the same retained included receipt, not a second message")
            acknowledged = self.acknowledge(recipient, receipt)
            self.check(acknowledged["duplicate"] is False,
                       "the first acknowledgement is committed once")
            repeated = self.acknowledge(recipient, receipt)
            self.check(repeated["duplicate"] is True,
                       "repeating acknowledgement reuses the same stable receipt")
            self.check(not self.inbox(recipient) and not self.receive(recipient),
                       "an acknowledged target has no replay in inbox or ordinary tool delivery")
            self.receipt_evidence.append({
                "recipient_id": recipient["id"], "client_id": recipient["client_id"],
                "kind": recipient["kind"], "text": body,
                "included": receipt, "acknowledged": acknowledged, "duplicate_ack": repeated,
            })
        self.check(len(received_ids) == 3, "the three exact targets have independent message receipts")
        self.replace_short(front, "/group status " + group["group_request_id"])
        front.send(ENTER)
        self.wait_state(front, lambda state: state["draft"]["text"] == "",
                        "open the retained group receipt report")
        front.until(lambda shot: "acknowledged" in text(shot), "the source displays recipient confirmations")
        self.capture(front, "acknowledged", "acknowledged")
        retry = "/group retry " + group["group_request_id"]
        self.replace_short(front, retry)
        front.send(ENTER)
        front.until(lambda shot: "No unconfirmed failed targets" in text(shot),
                    "accepted targets cannot be sent again by group retry")
        self.check(self.ui("/display/conversation")["draft"]["text"] == retry
                   and all(not self.inbox(item) and not self.receive(item) for item in recipients),
                   "retry keeps its rejected draft and never duplicates an accepted group")

        pending_body = "CLOSED_SOURCE_GROUP must never arrive after its source connection ends"
        pending = self.submit_group(front, pending_body, selected,
                                    previous=[group["group_request_id"]])
        # No recipient has made any tool or inbox call since this second send.
        front.disconnect()
        front.close()
        replacement = self.start_front("group-source-restarted")
        self.check(replacement.client_id != source_id,
                   "restarting only the owned source produces a fresh authenticated process identity")
        for recipient in recipients:
            self.check(not self.receive(recipient) and not self.inbox(recipient),
                       "queued messages from the disconnected source cannot bind to its replacement")
        self.replace_short(replacement, "/group retry " + pending["group_request_id"])
        replacement.send(ENTER)
        replacement.until(lambda shot: "Group receipt is no longer retained" in text(shot),
                          "a new source cannot resume the old group's send plan")
        self.check(all(not self.receive(item) and not self.inbox(item) for item in recipients),
                   "explicit old-plan retry after source restart also sends nothing")
        self.capture(replacement, "disconnected-no-replay", "Group receipt")
        self.disconnect_evidence = {
            "old_source_client_id": source_id, "new_source_client_id": replacement.client_id,
            "closed_group_request_id": pending["group_request_id"],
            "expected_recipient_ids": sorted(selected), "delivered_after_restart": 0,
            "old_plan_retry": "rejected",
        }
        self.check(self.queue()["queue"] == queue_before,
                   "group delivery and reconnect never create an ordinary user queue entry")
        return {"status": "passed", "transport": "owned PTY + production loopback account router",
                "identity_provider": "synthetic upstream GitHub fixture", "models_started": 0,
                "recipient_processes": 2, "exact_thread_recipients": 1}


def interrupted(_signal, _frame):
    raise TimeoutError("session group PTY acceptance interrupted; cleaning up owned processes")


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
        parser.error("all prebuilt executables must exist; this script never builds missing inputs")
    before = {name: digest(path) for name, path in binaries.items()}
    args.out.mkdir(parents=True, exist_ok=True)
    if (args.out / "report.json").exists():
        parser.error("use a new output directory to preserve prior acceptance evidence")
    os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(binaries["fixture"])
    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGINT, interrupted)
    started = time.monotonic()
    report = {"status": "failed", "source_revision": args.source_revision}
    with tempfile.TemporaryDirectory(prefix="ssmcp-session-groups-pty-",
                                     dir=args.out.resolve().parent) as temporary:
        harness = SessionGroups(Path(temporary), args.bin_dir.resolve(),
                                binaries["decoder"], args.out)

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
            print(report["traceback"], flush=True)
            harness.diagnostics()
        finally:
            try:
                harness.close_frontends()
            finally:
                try:
                    harness.cleanup()
                except Exception as error:
                    report["status"] = "failed"
                    report["cleanup_error"] = redact(f"{type(error).__name__}: {error}")
        after = {name: digest(path) for name, path in binaries.items()}
        if before != after:
            report["status"] = "failed"
            report["error"] = "a prebuilt executable changed during acceptance"
        report.update(
            checks=harness.checks, frames=harness.frames,
            groups=harness.group_evidence, receipts=harness.receipt_evidence,
            disconnect=harness.disconnect_evidence,
            elapsed_seconds=round(time.monotonic() - started, 3),
            binaries_sha256_before=before, binaries_sha256_after=after,
            inputs_unchanged=before == after,
            source_sha256={name: digest(Path(__file__).with_name(name)) for name in (
                "e2e_session_groups.py", "e2e_interactions.py", "e2e_input_history.py",
                "e2e_composer.py", "e2e_dashboard.py", "e2e_client_ui.py",
                "e2e_account_gateway.py", "e2e.py")})
        encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
        if redact(encoded) != encoded:
            raise AssertionError("the shareable report contains a private fixture credential")
        (args.out / "report.json").write_text(encoded, encoding="utf-8")
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "frames": len(report["frames"]), "elapsed_seconds": report["elapsed_seconds"]}))
    if report["status"] != "passed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
