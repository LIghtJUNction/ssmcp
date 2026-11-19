#!/usr/bin/env python3
"""Exercise the composer, agent recipients, and actual reexec in an owned PTY.

Build ssmcp, ssmcpd, and examples/vt_snapshot first, then run:
  python3 scripts/e2e_composer.py --bin-dir target/debug --report /tmp/composer.json

Reuses the isolated SSH/OAuth harness and independent VT decoder. All keys,
ports, processes, workspaces, and collaboration members belong to this test.
No live session, installed coding harness, or model is used. Native UI calls
are checked to be observational; screen waits never call a native work tool.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import tempfile
import time

from e2e_dashboard import Dashboard, sha256, text
from e2e_client_ui import column, empty_composer_ready


ESC = b"\x1b"
UP = b"\x1b[A"
DOWN = b"\x1b[B"
LEFT = b"\x1b[D"
RIGHT = b"\x1b[C"
ALT_LEFT = b"\x1b[1;3D"
ALT_RIGHT = b"\x1b[1;3C"
HOME = b"\x1b[H"
DELETE = b"\x1b[3~"
ENTER = b"\r"


def input_text(snapshot):
    # This scenario fixes the physical height at 58 rows: the last three rows
    # before the footer are the actual composer, not earlier log output.
    return "\n".join(snapshot["rows"][-4:-1])


def visible_record(snapshot, request_text, status, message_id=None, *, selected=False):
    rows = snapshot["rows"][:-4]
    expected = " ".join(request_text.split())
    for index, row in enumerate(rows):
        meta = re.match(r"^\s*([^·]+?)\s*·\s*#([^\s·]+)(?:\s|·|$)", row)
        if meta is None or (status is not None and meta[1].strip() != status):
            continue
        if message_id is not None and meta[2] != message_id[:8]:
            continue
        body = []
        marked = False
        for line in reversed(rows[max(0, index - 2):index]):
            if not line.strip() or "Requests ·" in line:
                break
            marked |= line.lstrip().startswith("›")
            body.insert(0, line.strip().removeprefix("›").strip())
        if expected in " ".join(body) and (not selected or marked):
            return True
    return False


class Composer(Dashboard):
    def config(self, name, **_options):
        # e2e_dashboard deliberately exercises split layout. This scenario
        # independently selects chat without changing that existing test.
        path = self.directory / f"{name}.toml"
        path.write_text(
            '[ui]\ndashboard=true\ncolor=false\nlog_format="pretty"\n'
            'timestamps=false\nlayout="chat"\nsidebar=false\n'
        )
        return path

    def response(self, code, *, agent=None, controller=None):
        arguments = {"code": code}
        if agent is not None:
            arguments["agent"] = agent
        if controller is not None:
            arguments["controller"] = controller
        status, body, _ = self.rpc(
            "tools/call", {"name": "code_mode", "arguments": arguments}, modern=True
        )
        if status != 200 or not isinstance(body, dict) or "error" in body:
            raise AssertionError(f"code_mode HTTP boundary failed: {status}, {body}")
        result = body["result"]
        if result.get("isError"):
            raise AssertionError(f"code_mode orchestration failed: {result}")
        return result

    @staticmethod
    def messages(response):
        return response.get("structuredContent", {}).get("user_messages", [])

    def observe(self, name, arguments=None):
        if not name.startswith("ssmcp.ui."):
            raise AssertionError("only native UI operations may be used for observation")
        result = self.response("return await call(%s, %s, %s);" % (
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments or {}),
        ))
        if self.messages(result):
            raise AssertionError("an unbound UI refresh consumed a queued user requirement")
        native = result["structuredContent"]["result"]
        if native.get("isError"):
            raise AssertionError(f"native UI operation failed: {native}")
        data = native["structuredContent"]
        if data.get("spilled"):
            raise AssertionError(f"{name} returned an artifact instead of UI state; "
                                 f"format={data.get('format')} artifact={data.get('artifact')}")
        return data

    def queue(self):
        return self.observe("ssmcp.ui.queue", {"action": "get"})

    def ui(self, path):
        projection = self.observe("ssmcp.ui.get", {"path": path})
        if projection.get("schema_version") != 1 or projection.get("path") != path or "value" not in projection:
            raise AssertionError(f"native UI projection did not return {path}")
        return projection["value"]

    def work(self, *, member=None, controller=None):
        code = "return await call(%s, 'terminal.status', {});" % json.dumps(self.client_id)
        if member is not None:
            code = "await collab.bind(%s, %s); %s" % (
                json.dumps(self.client_id), json.dumps({
                    "agent_id": member["agent_id"], "token": member["token"],
                }), code,
            )
        result = self.response(code, controller=controller)
        native = result["structuredContent"]["result"]
        if native.get("isError") or "user_messages" in native.get("structuredContent", {}):
            raise AssertionError("steering changed the native tool result envelope")
        return result

    @staticmethod
    def record(queue, message_id=None, request_text=None):
        matching = [record for record in queue["queue"] if
                    (message_id is None or record["id"] == message_id)
                    and (request_text is None or record["text"] == request_text)]
        if len(matching) != 1:
            raise AssertionError("expected one matching local request")
        return matching[0]

    def focus_input(self, front):
        front.send(ESC)
        # A standalone Escape has a deliberate short decoder timeout. Keep it
        # separate from the next text instead of accidentally creating Alt+key.
        front.pump(0.12)

    def command(self, front, command):
        self.focus_input(front)
        if self.ui("/display/conversation")["draft"]["text"]:
            raise AssertionError("local command helper would overwrite a nonempty draft")
        front.send(command + "\r")
        destination = {"/queue": ("queue", 4, 8), "/logs": ("logs", 7, 8)}[command]
        front.until(lambda shot: column(shot) == destination, f"local {command} column")

    def activate_column(self, front, name):
        state = self.ui("/display/conversation")["canvas"]
        index = state["columns"].index(name)
        distance = index - state["active"]
        if distance:
            front.send((ALT_RIGHT if distance > 0 else ALT_LEFT) * abs(distance))
        front.until(lambda shot: column(shot) == (name, index + 1, len(state["columns"])),
                    f"horizontal navigation to the existing {name} column")

    def submit(self, front, request_text):
        self.focus_input(front)
        # All scenario requirements are single logical lines. Ctrl+A/Ctrl+K
        # clears only test-owned text and does not depend on the current cursor.
        front.send(b"\x01\x0b" + request_text.encode() + ENTER)
        front.until(
            lambda shot: visible_record(shot, request_text, "queued")
                         and request_text not in input_text(shot),
            "the typed requirement becoming queued and its accepted draft clearing",
        )
        record = self.record(self.queue(), request_text=request_text)
        self.check(record["status"] == "queued", "typed input is acknowledged by the server queue")
        return record

    def select_request(self, front, message_id):
        queue = self.queue()
        indexes = [index for index, item in enumerate(queue["queue"]) if item["id"] == message_id]
        if len(indexes) != 1:
            raise AssertionError("cannot select missing or duplicated request")
        self.focus_input(front)
        front.send(b"\t" + HOME + DOWN * indexes[0])
        front.until(
            lambda shot: visible_record(shot, "", None, message_id, selected=True),
            "keyboard selection of the request card",
        )
        # The abbreviated visual ID is only a location hint. Verify the exact
        # selected identity through the authoritative projection and full queue.
        selected = self.ui("/display/conversation")["selected"]
        current = self.queue()["queue"]
        self.check(0 <= selected < len(current) and current[selected]["id"] == message_id,
                   "the selected request retains its exact full identity")

    def agents(self, front):
        self.focus_input(front)
        # Queue, audit and history are independent columns. Reach the existing
        # conversation first, then exercise ordinary Left at the draft edge.
        self.activate_column(front, "conversation")
        self.focus_input(front)
        draft = self.ui("/display/conversation")["draft"]
        if draft["cursor"] != 0:
            raise AssertionError("agent panel helper requires the start of the current draft")
        front.send(LEFT)
        front.until(lambda shot: column(shot) == ("agents", 1, 8) and shot["hide_cursor"],
                    "Left entering the adjacent Agents column")

    def recipient(self, front, member=None):
        self.agents(front)
        roster = self.queue()["agents"]
        if member is None:
            index = 0
            label = "next caller"
        else:
            index = 1 + next(index for index, agent in enumerate(roster)
                             if agent["agent_id"] == member["agent_id"])
            label = roster[index - 1]["name"]
        front.send(HOME + DOWN * index + ENTER)
        # The default recipient is shown in the compact footer; exact agents
        # retain the explicit recipient label above the input.
        visible_recipient = "To Next caller" if member is None else f"To · {label}"
        front.until(lambda shot: visible_recipient in text(shot)
                                 and column(shot) == ("conversation", 2, 8) and not shot["hide_cursor"],
                    "selected recipient returning to the conversation composer")
        self.check(self.ui("/display/conversation")["target"]
                   == (None if member is None else member["agent_id"]),
                   "Enter selects the exact member and closes the agent panel")
        self.command(front, "/queue")

    def expect_delivery(self, response, record, request_text=None, agent_id=None):
        messages = self.messages(response)
        if len(messages) != 1 or messages[0].get("message_id") != record["id"]:
            received = [message.get("message_id") for message in messages]
            raise AssertionError(f"expected delivery {record['id']}, received identities {received}")
        self.check(len(messages) == 1 and messages[0]["message_id"] == record["id"],
                   "only the expected queued identity is included in the tool response")
        expected = request_text if request_text is not None else record["text"]
        self.check(messages[0]["text"] == expected and messages[0].get("agent_id") == agent_id,
                   "delivery retains the exact revised text and authenticated recipient")
        self.check(any(item.get("type") == "text" and "USER REQUIREMENTS" in item.get("text", "")
                       and expected in item.get("text", "") for item in response["content"]),
                   "the tool response contains a conspicuous separate user-requirements text block")

    def wait_delivered(self, front, record, request_text=None):
        request_text = request_text if request_text is not None else record["text"]
        front.until(lambda shot: visible_record(shot, request_text, "delivered", record["id"]),
                    "the local queue observing response-boundary delivery")

    def new_credentials(self, front):
        deadline = time.monotonic() + 20
        while time.monotonic() < deadline:
            front.pump(0.05)
            if front.process.poll() is not None:
                raise AssertionError("the client exited instead of reexecing")
            if b"\n" not in front.stdout:
                continue
            line, _, remainder = front.stdout.partition(b"\n")
            front.stdout = bytearray(remainder)
            credentials = json.loads(line)
            self.validate_connection_record(credentials)
            return credentials
        raise AssertionError("local /reload did not reconnect within twenty seconds")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        # The UI's Git observer sees only this empty fixture repository. No
        # commit hooks, credential helpers, or existing checkout are consulted.
        subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "-q"],
                       cwd=self.workspace, env={**os.environ, **self.safe_git_env},
                       stdout=subprocess.DEVNULL,
                       stderr=subprocess.PIPE, check=True, timeout=5)
        front = self.frontend("composer", self.config("composer"))
        self.oauth_login(front.credentials)
        front.until(empty_composer_ready,
                    "the live chat composer in the owned terminal")
        self.command(front, "/queue")
        self.check(self.queue()["supported"], "the account relay negotiated the request-queue wire protocol")

        first_text = "touch composer-hidden-shell-leak"
        following_draft = "COMPOSER_DRAFT_TYPED_BEFORE_ACK"
        self.focus_input(front)
        # One physical write gives the client an Enter followed by a new draft
        # before an asynchronous queue acknowledgement can be handled.
        front.send(first_text + "\r" + following_draft)
        front.until(lambda shot: visible_record(shot, first_text, "queued")
                                 and following_draft in input_text(shot),
                    "an accepted requirement without erasing later typed text")
        first = self.record(self.queue(), request_text=first_text)
        self.check(not (self.workspace / "composer-hidden-shell-leak").exists(),
                   "typing a requirement never executes it in an invisible shell")
        self.check(self.ui("/display/conversation")["draft"]["text"]
                   == following_draft, "matching queue acknowledgement preserves the subsequently edited draft")
        self.check(self.record(self.queue(), message_id=first["id"])["status"] == "queued",
                   "repeated UI observation leaves the next-caller requirement pending")
        self.expect_delivery(self.work(), first)
        self.wait_delivered(front, first)
        self.check(not self.messages(self.work()), "a delivered request is not included in the next tool response again")

        second = self.submit(front, "COMPOSER_UP_EDIT_ORIGINAL")
        front.send(UP)
        front.until(lambda shot: "Editing · next caller" in text(shot)
                                 and second["text"] in input_text(shot),
                    "Up reclaiming the pending request before editing")
        self.check(self.record(self.queue(), message_id=second["id"])["status"] == "editing",
                   "Up obtains a server-confirmed takeback before replacing queued text")
        self.check(not self.messages(self.work()), "a request held for editing is excluded from delivery")
        revised = "COMPOSER_UP_EDIT_REVISED 界"
        front.send(b"\x01\x0b" + revised.encode() + ENTER)
        front.until(lambda shot: visible_record(shot, revised, "queued", second["id"])
                                 and revised not in input_text(shot),
                    "the edited text queued under its original identity")
        edited = self.record(self.queue(), message_id=second["id"])
        self.check(edited["text"] == revised and edited["revision"] > second["revision"],
                   "editing advances the CAS revision while retaining the same request ID")
        self.expect_delivery(self.work(), second, revised)
        self.wait_delivered(front, second, revised)

        third = self.submit(front, "COMPOSER_ENTER_EDIT_ORIGINAL")
        parked = "COMPOSER_PARKED_DRAFT 界"
        self.focus_input(front)
        front.send(parked)
        self.select_request(front, third["id"])
        front.send(ENTER)
        front.until(lambda shot: "Editing · next caller" in text(shot)
                                 and third["text"] in input_text(shot),
                    "queue Enter reclaiming the selected request")
        revised_third = "COMPOSER_ENTER_EDIT_REVISED"
        front.send(b"\x01\x0b" + revised_third.encode() + ENTER)
        front.until(lambda shot: visible_record(shot, revised_third, "queued", third["id"])
                                 and parked in input_text(shot),
                    "the original parked draft restored after matching edit acknowledgement")
        self.expect_delivery(self.work(), third, revised_third)
        self.wait_delivered(front, third, revised_third)
        self.check(self.ui("/display/conversation")["draft"]["text"] == parked,
                   "queue Enter preserves an unrelated draft through edit, acceptance, and delivery")

        cancelled = self.submit(front, "COMPOSER_CANCEL_BEFORE_DELIVERY")
        self.select_request(front, cancelled["id"])
        front.send(DELETE)
        front.until(lambda shot: visible_record(shot, cancelled["text"], "cancelled", cancelled["id"]),
                    "Delete cancelling the selected queued requirement")
        self.check(not self.messages(self.work()), "a confirmed cancellation never reaches the next native tool response")

        # Exercise the same atomic interface available to alternate frontends.
        atomic = self.observe("ssmcp.ui.queue", {"action": "submit", "text": "COMPOSER_ATOMIC_UI_REQUEST"})
        front.until(lambda shot: visible_record(shot, "COMPOSER_ATOMIC_UI_REQUEST", "queued", atomic["id"]),
                    "atomic UI queue submission visible in the physical client")
        atomic_record = self.record(self.queue(), message_id=atomic["id"])
        self.check(atomic["requested"] and atomic_record["status"] == "queued",
                   "native ssmcp.ui.queue publishes asynchronously without claiming its own request")
        self.select_request(front, atomic["id"])
        front.send(DELETE)
        front.until(lambda shot: visible_record(shot, atomic_record["text"], "cancelled", atomic["id"]),
                    "the keyboard cancelling an alternate frontend's queued request")

        members = []
        for role, status in (("review", "reviewing"), ("build", "testing")):
            member = self.public_call("collab", {"action": "join", "client_id": self.client_id,
                                                  "name": "Shared name", "role": role})
            self.sensitive.append(member["token"])
            self.public_call("collab", {"action": "update", "client_id": self.client_id,
                                         "agent_id": member["agent_id"], "token": member["token"],
                                         "status": status})
            members.append(member)
        self.agents(front)
        front.until(lambda shot: "reviewing" in text(shot) and "testing" in text(shot),
                    "the two real public collaboration statuses")
        roster = self.queue()["agents"]
        self.check(len(roster) == 2 and {agent["status"] for agent in roster} == {"reviewing", "testing"}
                   and all(member["token"] not in json.dumps(roster) for member in members),
                   "the roster shows actual members and public status without participant tokens")
        front.send(ESC)
        front.until(lambda shot: column(shot) == ("conversation", 2, 8) and not shot["hide_cursor"],
                    "Escape returning from agents to the default conversation")
        navigation_draft = "保留草稿 界"
        front.send(navigation_draft.encode() + LEFT)
        front.until(lambda shot: navigation_draft in input_text(shot),
                    "a Unicode draft with ordinary Left cursor movement")
        navigation = self.ui("/display/conversation")
        self.check(navigation["view"] == "activity" and navigation["focus"] == "input"
                   and navigation["draft"] == {"text": navigation_draft,
                                                "cursor": len(navigation_draft.encode()) - len("界".encode())},
                   "Left within nonempty UTF-8 input moves the cursor without opening agents")
        front.send(b"\x01" + LEFT)
        front.until(lambda shot: column(shot) == ("agents", 1, 8) and navigation_draft in input_text(shot),
                    "Left at the start of a draft opening the agent panel")
        before_choice = self.ui("/display/conversation")
        self.check(before_choice["draft"] == {"text": navigation_draft, "cursor": 0}
                   and before_choice["target"] is None and before_choice["editing"] is None,
                   "opening agents preserves the draft and does not choose or send a recipient")
        front.send(ESC)
        front.until(lambda shot: column(shot) == ("conversation", 2, 8) and not shot["hide_cursor"],
                    "Escape closing the panel without clearing the Unicode draft")
        after_escape = self.ui("/display/conversation")
        self.check(after_escape["draft"] == before_choice["draft"] and after_escape["target"] is None,
                   "Escape restores the conversation with exactly the same UTF-8 cursor and recipient")
        self.agents(front)
        selected_index = 1 + next(index for index, item in enumerate(self.queue()["agents"])
                                  if item["agent_id"] == members[0]["agent_id"])
        front.send(HOME + DOWN * selected_index + RIGHT)
        front.until(lambda shot: column(shot) == ("conversation", 2, 8)
                                 and navigation_draft in input_text(shot) and not shot["hide_cursor"],
                    "Right moving to the conversation without choosing a member")
        navigated = self.ui("/display/conversation")
        self.check(navigated["target"] is None and navigated["draft"] == before_choice["draft"],
                   "Right changes only the active column, preserving the draft and recipient")
        self.agents(front)
        self.check(self.ui("/display/conversation")["selected_agent"] == "member:" + members[0]["agent_id"],
                   "horizontal navigation preserves the independently highlighted member")
        front.send(ENTER)
        front.until(lambda shot: column(shot) == ("conversation", 2, 8)
                                 and "To · Shared name" in text(shot) and not shot["hide_cursor"],
                    "Enter choosing the member and returning to the conversation")
        chosen = self.ui("/display/conversation")
        self.check(chosen["target"] == members[0]["agent_id"]
                   and chosen["draft"] == before_choice["draft"],
                   "Enter changes only the authenticated recipient while preserving the draft")
        self.agents(front)
        self.check(self.ui("/display/conversation")["selected_agent"] == "member:" + members[0]["agent_id"],
                   "reopening agents highlights the currently selected member")
        front.send(ESC)
        front.until(lambda shot: column(shot) == ("conversation", 2, 8) and not shot["hide_cursor"],
                    "the selected member conversation restored")
        front.send(b"\x01\x0b")
        front.until(lambda shot: "Message the agent" in input_text(shot), "the navigation fixture draft cleared")
        self.recipient(front, members[0])
        exact = self.submit(front, "COMPOSER_EXACT_AGENT_REQUEST")
        self.check(exact["target"] == members[0]["agent_id"], "recipient selection uses the registered agent ID")
        self.check(not self.messages(self.work(controller={"name": "Shared name"})),
                   "matching a displayed controller name does not authenticate an exact recipient")
        self.check(not self.messages(self.work(member=members[1])),
                   "another authenticated member cannot take an exact-agent requirement")
        self.expect_delivery(self.work(member=members[0]), exact, agent_id=members[0]["agent_id"])
        self.wait_delivered(front, exact)

        retargeted = self.submit(front, "COMPOSER_RETARGET_ORIGINAL")
        front.send(UP)
        front.until(lambda shot: "Editing · Shared name" in text(shot)
                                 and retargeted["text"] in input_text(shot),
                    "an exact-agent requirement taken back for recipient editing")
        # Keep the held edit while navigating from Queue through the canvas;
        # the ordinary Left boundary action enters Agents from conversation.
        front.send(b"\x01")
        self.agents(front)
        held = self.ui("/display/conversation")
        self.check(held["editing"] == retargeted["id"]
                   and held["draft"] == {"text": retargeted["text"], "cursor": 0},
                   "Left preserves the held request identity and original text during retargeting")
        front.send(HOME + ENTER)
        front.until(lambda shot: "Editing · next caller" in text(shot)
                                 and column(shot) == ("conversation", 2, 8) and not shot["hide_cursor"],
                    "the explicit Next caller choice returning to the active edit")
        retargeted_text = "COMPOSER_RETARGET_NEXT_CALLER"
        front.send(b"\x01\x0b" + retargeted_text.encode() + ENTER)
        front.until(lambda shot: "Queued" in shot["rows"][-1]
                                 and retargeted_text not in input_text(shot),
                    "publication of the edited recipient and text")
        self.command(front, "/queue")
        front.until(lambda shot: visible_record(shot, retargeted_text, "queued", retargeted["id"]),
                    "the same request identity returning to the queue")
        retarget_record = self.record(self.queue(), message_id=retargeted["id"])
        self.check(retarget_record["target"] == "next" and retarget_record["text"] == retargeted_text,
                   "choosing Next caller during an exact-agent edit replaces the previous target")
        self.expect_delivery(self.work(), retargeted, retargeted_text)
        self.wait_delivered(front, retargeted, retargeted_text)
        self.recipient(front)

        handoff = self.submit(front, "COMPOSER_QUEUED_ACROSS_RELOAD")
        self.focus_input(front)
        front.send(parked.encode() + LEFT * 2)
        front.until(lambda shot: parked in input_text(shot), "the unsent draft before parking")
        parked_cursor = self.queue()["draft"]["cursor"]
        # R copies delivered history into a fresh draft while parking this
        # unrelated text. /reload must restore that parked draft before reexec.
        self.select_request(front, first["id"])
        front.send(b"r")
        front.until(lambda shot: first_text in input_text(shot), "a delivered history item copied to a new draft")
        before = self.queue()
        self.check(before["draft_context"]["parked"]["text"] == parked
                   and before["draft_context"]["parked"]["cursor"] == parked_cursor,
                   "the unsent draft is parked before invoking local reload")
        page = self.observe("ssmcp.ui.get", {"path": "/steering/queue", "offset": 0, "limit": 2})
        self.check(page["total"] == len(before["queue"]) and len(page["value"]) == 2
                   and page["next_offset"] == 2 and page["value"][0]["id"] == first["id"],
                   "bounded UI projection pages the live queue without spilling or claiming a requirement")
        # The second copy would otherwise overwrite the single parked slot.
        # Do not use focus_input here: Escape is intentionally the restore key.
        front.send(b"\tr")
        front.until(lambda shot: "parked" in shot["rows"][-1].lower(),
                    "a repeated R refusing to overwrite the already parked draft")
        repeated = self.queue()
        self.check(repeated["draft"]["text"] == first_text
                   and repeated["draft_context"]["parked"]["text"] == parked,
                   "repeated resend-copy cannot overwrite either current or parked text")
        front.send(ESC)
        front.until(lambda shot: parked in input_text(shot), "Escape restoring the parked draft")
        escaped = self.queue()
        self.check(escaped["draft"]["text"] == parked and escaped["draft"]["cursor"] == parked_cursor
                   and escaped["draft_context"]["parked"] is None
                   and escaped["draft_context"]["target"] is None,
                   "Escape restores the parked request, recipient, and UTF-8 cursor")
        self.select_request(front, first["id"])
        front.send(b"r")
        front.until(lambda shot: first_text in input_text(shot), "a fresh copy before the reload command")
        old_id, old_token, old_pid = self.client_id, self.secret, front.process.pid
        front.send(b"\x01\x0b/reload\r")
        credentials = self.new_credentials(front)
        self.check(front.process.pid == old_pid and credentials["client_id"] != old_id,
                   "local /reload reexecs the same process and reconnects under a new session ID")
        front.credentials = credentials
        front.client_id = self.client_id = credentials["client_id"]
        front.until(lambda shot: parked in input_text(shot), "the parked draft restored in the restarted client")
        # A device reconnect replaces its client ID, while the user's independent
        # account authorization remains valid for the newly registered device.
        status, _, _ = self.rpc("ping", modern=True, secret=old_token)
        self.check(status == 200 and self.secret == old_token,
                   "reexec preserves the independently authorized account OAuth grant")
        listed = self.public_call("list", secret=old_token)["clients"]
        self.check(any(item["id"] == self.client_id for item in listed)
                   and all(item["id"] != old_id for item in listed),
                   "the same account grant sees the replacement without the disconnected client ID")
        restored = self.queue()
        self.check(restored["draft"]["text"] == parked and restored["draft"]["cursor"] == parked_cursor
                   and restored["draft_context"]["parked"] is None,
                   "reexec restores parked text as the visible draft rather than stranding it in hidden state")
        self.check(self.record(restored, message_id=first["id"])["status"] == "delivered",
                   "delivered history remains delivered after the room changes")
        # Inspect the existing Queue column without writing a slash command
        # into the restored draft or relying on the old F2 view order.
        self.activate_column(front, "queue")
        front.until(lambda shot: column(shot) == ("queue", 4, 8)
                                 and visible_record(shot, handoff["text"], "queued", handoff["id"]),
                    "confirmed undelivered work republished after the reload handoff")
        # Restored records are locally queued at revision zero until the client
        # publishes them. Wait for a server acknowledgement before requiring a
        # subsequent response to include that request; UI polling is observational.
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            resumed = self.record(self.queue(), message_id=handoff["id"])
            if resumed["status"] == "queued" and resumed["revision"] > 0:
                break
            front.pump(0.05)
        else:
            raise AssertionError(f"reload replay was not acknowledged: status={resumed['status']} revision={resumed['revision']}")
        self.check(resumed["room_id"] == self.client_id and resumed["target"] == "next",
                   "planned reload retains the safe queued identity and binds it to the replacement room")
        self.expect_delivery(self.work(), handoff)
        self.wait_delivered(front, handoff)
        self.check(not self.messages(self.work()), "reload never replays previously delivered requirements")
        self.check(self.ui("/display/conversation")["draft"]["text"] == parked,
                   "background replay acknowledgements do not clear the restored local draft")
        self.check(not (self.workspace / "composer-hidden-shell-leak").exists(),
                   "all composer and reload interactions leave the hidden shell untouched")
        final, restored_termios = front.disconnect()
        self.check(not final["alternate_screen"] and restored_termios == front.original_termios,
                   "normal exit restores the owned terminal after a real client reexec")
        self.check(front.process.returncode == 0, "the reexecuted client disconnects successfully")
        self.metrics["composer"] = {"bytes": len(front.transcript),
                                    "sha256": hashlib.sha256(front.transcript).hexdigest(),
                                    "request_count": len(restored["queue"])}
        front.close()


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("composer E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--decoder", type=Path, help="defaults to BIN_DIR/examples/vt_snapshot")
    parser.add_argument("--report", type=Path, help="write assertions and hashes without credentials or raw PTY data")
    args = parser.parse_args()
    decoder = args.decoder or args.bin_dir / "examples/vt_snapshot"
    for executable in (args.bin_dir / "ssmcp", args.bin_dir / "ssmcpd", decoder):
        if not executable.is_file():
            parser.error(f"missing {executable}; build the binaries and vt_snapshot first")
    started = time.monotonic()
    report = {"status": "failed", "platform": platform.system(),
              "transport": "production account router over loopback HTTP/WebSocket, synthetic GitHub provider",
              "scope": "real composer, queue revisions, bound recipients, and local reexec; no models",
              "binaries": {name: sha256(path) for name, path in (
                  ("ssmcp", args.bin_dir / "ssmcp"), ("ssmcpd", args.bin_dir / "ssmcpd"), ("vt_snapshot", decoder))},
              "sources": {path.name: sha256(path) for path in (
                  Path(__file__), Path(__file__).with_name("e2e.py"),
                  Path(__file__).with_name("e2e_dashboard.py"),
                  Path(__file__).with_name("e2e_client_ui.py"))}}
    with tempfile.TemporaryDirectory(prefix="ssmcp-composer-e2e-") as directory:
        harness = Composer(Path(directory), args.bin_dir, decoder)
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
    print(f"\nPassed {len(report['checks'])} composer checks in {report['duration_seconds']}s", flush=True)


if __name__ == "__main__":
    main()
