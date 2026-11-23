#!/usr/bin/env python3
"""Exercise the composer, agent recipients, and actual reexec in an owned PTY.

Build ssmcp, ssmcpd, and examples/vt_snapshot first, then run:
  python3 scripts/e2e_composer.py --bin-dir target/debug --report /tmp/composer.json

Reuses the isolated account/OAuth harness and independent VT decoder. All keys,
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
import traceback

from e2e_account_relay import wait_for
from e2e_dashboard import Dashboard, sha256, text
from e2e_client_ui import body_text, column, empty_composer_ready, left_rows, view


COMPOSER_TIMEOUT_SECONDS = 240

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


def editing_queue(snapshot, recipient):
    title = left_rows(snapshot)[-5].partition("─")[0].strip()
    return title == f"Edit queue · Enter sends · {recipient}"


def visible_record(snapshot, request_text, status, message_id=None, *, selected=False):
    rows = snapshot["rows"][:-4]
    expected = " ".join(request_text.split())
    receipts = {"queued": "○ Waiting", "delivered": "● Read", "cancelled": "○ Cancelled"}
    expected_receipt = receipts[status] if status is not None else None
    for index, row in enumerate(rows):
        meta = re.match(r"^\s*([○●] [^·]+?)(?: · Enter edit)? · #([^\s·]+)(?:\s|·|$)", row)
        if meta is None or (expected_receipt is not None and meta[1] != expected_receipt):
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
    # Keep the router available for graceful stop after the scenario deadline.
    fixture_timeout_seconds = COMPOSER_TIMEOUT_SECONDS + 30

    def client_environment(self, label, **options):
        environment = super().client_environment(label, **options)
        root = (self.directory / "composer-environment").resolve()
        root.mkdir(mode=0o700, exist_ok=True)
        homes = {
            "HOME": root / "home",
            "XDG_STATE_HOME": root / "state",
            "XDG_CONFIG_HOME": root / "config",
            "XDG_DATA_HOME": root / "data",
            "XDG_CACHE_HOME": root / "cache",
            "XDG_RUNTIME_DIR": root / "runtime",
        }
        for path in homes.values():
            path.mkdir(mode=0o700, exist_ok=True)
        self.composer_state_home = homes["XDG_STATE_HOME"]
        environment.update({name: str(path) for name, path in homes.items()})
        return environment

    def config(self, name, **_options):
        # e2e_dashboard deliberately exercises split layout. This scenario
        # independently selects chat without changing that existing test.
        path = self.directory / f"{name}.toml"
        path.write_text(
            '[ui]\ndashboard=true\ncolor=false\nlog_format="pretty"\n'
            'timestamps=false\nlayout="chat"\nsidebar=false\n'
        )
        return path

    def rpc(self, method, params=None, **options):
        # Capture the actual code_mode HTTP boundary once, including callers
        # that bypass response() through Harness.call/live_event_call.
        if not hasattr(self, "gateway_deliveries"):
            self.gateway_deliveries = []
        observed_target = self.client_id
        status, body, headers = super().rpc(method, params, **options)
        if (method == "tools/call" and isinstance(params, dict)
                and params.get("name") == "code_mode" and isinstance(body, dict)):
            result = body.get("result")
            structured = result.get("structuredContent") if isinstance(result, dict) else None
            if isinstance(structured, dict):
                messages = structured.get("user_messages", [])
                if not isinstance(messages, list) or any(not isinstance(item, dict) for item in messages):
                    raise AssertionError("code_mode returned an invalid user-requirements envelope")
                self.gateway_deliveries.extend({
                    # This is the fixture's observation target, not a client ID
                    # returned by the server. Real scope is checked by ui.events.
                    "observed_target_client_id": observed_target,
                    "message_id": item.get("message_id"),
                    "text": item.get("text"),
                    "agent_id": item.get("agent_id"),
                } for item in messages)
        return status, body, headers

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
        destination = {"/queue": "queue", "/logs": "logs", "/sessions": "recipients"}[command]
        front.until(lambda shot: view(shot, destination), f"local {command} column")

    def activate_column(self, front, name):
        state = self.ui("/display/conversation")["canvas"]
        index = state["columns"].index(name)
        distance = index - state["active"]
        if distance:
            front.send((ALT_RIGHT if distance > 0 else ALT_LEFT) * abs(distance))
        expected = (name, index + 1, len(state["columns"]))
        front.until(lambda shot: column(shot) == expected
                    or (name == "conversation" and view(shot, "conversation")),
                    f"horizontal navigation to the existing {name} column")
        observed = self.ui("/display/conversation")["canvas"]
        if observed["active"] != index or observed["columns"] != state["columns"]:
            raise AssertionError("horizontal navigation changed the canvas or reached the wrong column")

    def wait_submitted(self, front, request_text, *, next_draft="", waiting=False):
        draft = next_draft or "Message the agent"
        front.until(lambda shot: view(shot, "conversation")
                                 and request_text in body_text(shot)
                                 and shot["rows"][-1].lstrip().startswith("Queued ·")
                                 and request_text not in input_text(shot)
                                 and draft in input_text(shot)
                                 and (not waiting or "○ 1 waiting ·" in left_rows(shot)[-5]),
                    "the submitted conversation body, queue acknowledgement and editor state")

    def submit(self, front, request_text):
        self.focus_input(front)
        # All scenario requirements are single logical lines. Ctrl+A/Ctrl+K
        # clears only test-owned text and does not depend on the current cursor.
        front.send(b"\x01\x0b" + request_text.encode() + ENTER)
        self.wait_submitted(front, request_text)
        self.activate_column(front, "queue")
        record = self.record(self.queue(), request_text=request_text)
        front.until(lambda shot: visible_record(shot, request_text, "queued", record["id"])
                                 and request_text not in input_text(shot),
                    "the accepted request visible in the queue")
        self.check(record["status"] == "queued", "typed input is acknowledged by the server queue")
        self.focus_input(front)
        front.until(lambda shot: view(shot, "queue") and not shot["hide_cursor"],
                    "the visible Queue composer regaining input focus")
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
        self.activate_column(front, "conversation")
        self.focus_input(front)
        state = self.ui("/display/conversation")
        if state["draft"]["cursor"] != 0:
            raise AssertionError("recipient helper requires the start of the current draft")
        if "recipients" not in state["canvas"]["columns"]:
            # Initialize through the public command only while the test-owned
            # composer is empty; later visits navigate without touching edits.
            if state["draft"]["text"] or state["editing"] is not None:
                raise AssertionError("open /sessions before composing or editing a request")
            self.command(front, "/sessions")
        else:
            self.activate_column(front, "recipients")
        front.until(lambda shot: view(shot, "recipients") and shot["hide_cursor"],
                    "the explicitly opened recipient picker")

    def highlight_recipient(self, front, member=None):
        # Local steering recipients precede observed/native process threads.
        # Verify the full highlighted key before Enter or horizontal navigation;
        # a matching display name or stale flat-row index cannot choose a peer.
        roster = self.queue()["agents"]
        if member is None:
            index = 0
            label = "This process · Next caller"
            selected_key = "member:next"
        else:
            index = 1 + next(index for index, agent in enumerate(roster)
                             if agent["agent_id"] == member["agent_id"])
            label = roster[index - 1]["name"]
            selected_key = "member:" + member["agent_id"]
        front.send(HOME + DOWN * index)
        front.until(
            lambda shot: view(shot, "recipients") and shot["hide_cursor"]
            and self.ui("/display/conversation")["selected_agent"] == selected_key,
            "the exact local recipient highlighted in Sessions",
        )
        return label

    def recipient(self, front, member=None):
        self.agents(front)
        label = self.highlight_recipient(front, member)
        front.send(ENTER)
        # The process row names the ordinary Next caller queue; exact members
        # retain their explicit recipient label above the input.
        visible_recipient = f"To {label}"
        front.until(lambda shot: visible_recipient in text(shot)
                                 and view(shot, "conversation") and not shot["hide_cursor"],
                    "selected recipient returning to the conversation composer")
        selected = self.ui("/display/conversation")
        self.check(selected["target"] == ("next" if member is None else member["agent_id"])
                   and selected["native_recipient"] is None,
                   "Enter selects the exact steering recipient and closes Sessions")
        self.command(front, "/queue")

    def expect_delivery(self, response, record, request_text=None, agent_id=None):
        messages = self.messages(response)
        if len(messages) != 1 or messages[0].get("message_id") != record["id"]:
            received = [message.get("message_id") for message in messages]
            raise AssertionError(f"expected delivery {record['id']}, received identities {received}")
        self.check(len(messages) == 1 and messages[0]["message_id"] == record["id"],
                   "only the expected queued identity is included in the tool response")
        self.check(messages[0]["client_id"] == record["room_id"]
                   and messages[0]["session_id"] == record["room_id"]
                   and messages[0]["thread_id"] == agent_id,
                   "the actual HTTP delivery identifies the queued room and authenticated recipient thread")
        expected = request_text if request_text is not None else record["text"]
        self.check(messages[0]["text"] == expected and messages[0].get("agent_id") == agent_id,
                   "delivery retains the exact revised text and authenticated recipient")
        self.check(any(item.get("type") == "text" and "USER REQUIREMENTS" in item.get("text", "")
                       and expected in item.get("text", "") for item in response["content"]),
                   "the tool response contains a conspicuous separate user-requirements text block")

    def check_post_delivery_journal(self, baseline, forbidden, *, marker, limit=16):
        # Exact-recipient input is verified at the HTTP delivery boundary. Use a
        # separate public probe to verify that the live process journal still works.
        receipt = self.observe("ssmcp.ui.message", {"text": marker})
        self.check(receipt.get("recorded") is True and receipt.get("storage") == "memory"
                   and bool(receipt.get("message_id")),
                   "the post-delivery journal probe has a real in-memory receipt")
        current = self.live_events(limit=limit)
        self.check(current["generation"] == baseline["generation"],
                   "post-delivery journal keeps the replacement process generation")
        self.check(current["scope"] == baseline["scope"],
                   "post-delivery journal keeps the replacement client scope")
        self.check(current["next_cursor"] >= baseline["next_cursor"],
                   "the replacement journal cursor advances monotonically")
        serialized = json.dumps(current["events"], ensure_ascii=False)
        matches = [event for event in current["events"]
                   if event.get("value", {}).get("message_id") == receipt["message_id"]]
        old_matches = sum(marker in serialized for marker in forbidden)
        self.check(old_matches == 0,
                   f"post-delivery journal excludes old bodies and identities (matches={old_matches})")
        self.check(len(matches) == 1 and matches[0]["value"].get("type") == "ssmcp.message"
                   and matches[0]["value"].get("text") == marker
                   and baseline["next_cursor"] <= matches[0]["sequence"] < current["next_cursor"],
                   "post-delivery journal contains the exact new probe inside its snapshot "
                   f"(matching_ids={len(matches)}, events={len(current['events'])}, "
                   f"cursor={baseline['next_cursor']}..{current['next_cursor']})")
        return current

    def wait_delivered(self, front, record, request_text=None):
        request_text = request_text if request_text is not None else record["text"]
        self.activate_column(front, "queue")
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
        initial_composition = self.ui("/display/conversation")
        initial_recipient = {key: initial_composition[key] for key in ("target", "native_recipient")}
        default_columns = ["statistics", "conversation", "code audit", "queue", "questions", "Logs", "Tasks"]
        self.check(initial_composition["canvas"]["columns"] == default_columns
                   and initial_composition["canvas"]["active"] == 1,
                   "the live client starts on Conversation in the exact seven-column default canvas")
        self.command(front, "/queue")
        self.check(self.queue()["supported"], "the account relay negotiated the request-queue wire protocol")

        first_text = "touch composer-hidden-shell-leak"
        following_draft = "COMPOSER_DRAFT_TYPED_BEFORE_ACK"
        self.focus_input(front)
        # One physical write gives the client an Enter followed by a new draft
        # before an asynchronous queue acknowledgement can be handled.
        front.send(first_text + "\r" + following_draft)
        self.wait_submitted(front, first_text, next_draft=following_draft, waiting=True)
        self.activate_column(front, "queue")
        first = self.record(self.queue(), request_text=first_text)
        front.until(lambda shot: visible_record(shot, first_text, "queued", first["id"])
                                 and following_draft in input_text(shot),
                    "the accepted requirement visible in the queue without erasing the later draft")
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
        front.until(lambda shot: editing_queue(shot, "next caller")
                                 and second["text"] in input_text(shot),
                    "Up reclaiming the pending request before editing")
        self.check(self.record(self.queue(), message_id=second["id"])["status"] == "editing",
                   "Up obtains a server-confirmed takeback before replacing queued text")
        self.check(not self.messages(self.work()), "a request held for editing is excluded from delivery")
        revised = "COMPOSER_UP_EDIT_REVISED 界"
        front.send(b"\x01\x0b" + revised.encode() + ENTER)
        self.wait_submitted(front, revised)
        self.activate_column(front, "queue")
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
        front.until(lambda shot: editing_queue(shot, "next caller")
                                 and third["text"] in input_text(shot),
                    "queue Enter reclaiming the selected request")
        revised_third = "COMPOSER_ENTER_EDIT_REVISED"
        front.send(b"\x01\x0b" + revised_third.encode() + ENTER)
        self.wait_submitted(front, revised_third, next_draft=parked)
        self.activate_column(front, "queue")
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
        expected_statuses = {}
        subtitles = []
        for role, status in (("review", "reviewing"), ("build", "testing")):
            member = self.public_call("collab", {"action": "join", "client_id": self.client_id,
                                                  "name": "Shared name", "role": role})
            self.sensitive.append(member["token"])
            self.public_call("collab", {"action": "update", "client_id": self.client_id,
                                         "agent_id": member["agent_id"], "token": member["token"],
                                         "status": status})
            members.append(member)
            expected_statuses[member["agent_id"]] = status
            subtitles.append(f"…{member['agent_id'][-8:]} · {role}")
        self.agents(front)
        recipient_canvas = self.ui("/display/conversation")["canvas"]
        self.check(recipient_canvas["columns"] == default_columns[:2] + ["recipients"] + default_columns[2:]
                   and recipient_canvas["active"] == 2,
                   "explicit Sessions inserts only Recipients as the third column of the eight-column canvas")
        front.until(lambda shot: all(subtitle in text(shot) for subtitle in subtitles),
                    "the public summaries beside the two same-name threads' ID suffixes")

        def matching_roster():
            roster = self.queue()["agents"]
            if (len(roster) == len(expected_statuses)
                    and {agent["agent_id"]: agent["status"] for agent in roster} == expected_statuses):
                return roster
            return None

        roster = wait_for(matching_roster, "the exact public thread IDs and statuses")
        self.check(all(member["token"] not in json.dumps(roster) for member in members),
                   "the roster shows the registered threads and their statuses without participant tokens")
        front.send(ESC)
        front.until(lambda shot: view(shot, "conversation") and not shot["hide_cursor"],
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
        front.until(lambda shot: view(shot, "statistics") and navigation_draft in input_text(shot),
                    "Left at the start of a draft opening Statistics")
        before_choice = self.ui("/display/conversation")
        self.check(before_choice["draft"] == {"text": navigation_draft, "cursor": 0}
                   and before_choice["target"] is None and before_choice["editing"] is None,
                   "opening Statistics preserves the draft and does not choose or send a recipient")
        front.send(ESC)
        front.until(lambda shot: view(shot, "conversation") and not shot["hide_cursor"],
                    "Escape closing the panel without clearing the Unicode draft")
        after_escape = self.ui("/display/conversation")
        self.check(after_escape["draft"] == before_choice["draft"] and after_escape["target"] is None,
                   "Escape restores the conversation with exactly the same UTF-8 cursor and recipient")
        self.agents(front)
        self.highlight_recipient(front, members[0])
        front.send(ALT_LEFT)
        front.until(lambda shot: view(shot, "conversation")
                                 and navigation_draft in input_text(shot) and not shot["hide_cursor"],
                    "Alt+Left returning from Recipients without choosing a member")
        navigated = self.ui("/display/conversation")
        self.check(navigated["target"] is None and navigated["draft"] == before_choice["draft"],
                   "horizontal navigation changes only the active column, preserving the draft and recipient")
        self.agents(front)
        self.check(self.ui("/display/conversation")["selected_agent"] == "member:" + members[0]["agent_id"],
                   "horizontal navigation preserves the independently highlighted member")
        front.send(ENTER)
        front.until(lambda shot: view(shot, "conversation")
                                 and "To Shared name" in text(shot) and not shot["hide_cursor"],
                    "Enter choosing the member and returning to the conversation")
        chosen = self.ui("/display/conversation")
        self.check(chosen["target"] == members[0]["agent_id"]
                   and chosen["draft"] == before_choice["draft"],
                   "Enter changes only the authenticated recipient while preserving the draft")
        self.agents(front)
        self.check(self.ui("/display/conversation")["selected_agent"] == "member:" + members[0]["agent_id"],
                   "reopening agents highlights the currently selected member")
        front.send(ESC)
        front.until(lambda shot: view(shot, "conversation") and not shot["hide_cursor"],
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
        front.until(lambda shot: editing_queue(shot, "Shared name")
                                 and retargeted["text"] in input_text(shot),
                    "an exact-agent requirement taken back for recipient editing")
        # Keep the held edit while navigating from Queue through the canvas;
        # the explicitly opened recipient column retains the held draft.
        front.send(b"\x01")
        self.agents(front)
        held = self.ui("/display/conversation")
        self.check(held["editing"] == retargeted["id"]
                   and held["target"] == members[0]["agent_id"]
                   and held["draft"] == {"text": retargeted["text"], "cursor": 0},
                   "recipient navigation preserves the held request, exact recipient and original text during retargeting")
        front.send(HOME + ENTER)
        front.until(lambda shot: editing_queue(shot, "Next caller")
                                 and view(shot, "conversation") and not shot["hide_cursor"],
                    "the explicit Next caller choice returning to the active edit")
        retargeted_text = "COMPOSER_RETARGET_NEXT_CALLER"
        front.send(b"\x01\x0b" + retargeted_text.encode() + ENTER)
        self.wait_submitted(front, retargeted_text)
        self.command(front, "/queue")
        front.until(lambda shot: visible_record(shot, retargeted_text, "queued", retargeted["id"]),
                    "the same request identity returning to the queue")
        retarget_record = self.record(self.queue(), message_id=retargeted["id"])
        self.check(retarget_record["target"] == "next" and retarget_record["text"] == retargeted_text,
                   "choosing Next caller during an exact-agent edit replaces the previous target")
        self.expect_delivery(self.work(), retargeted, retargeted_text)
        self.wait_delivered(front, retargeted, retargeted_text)
        self.recipient(front)

        legacy_root = self.composer_state_home / "ssmcp" / "conversations"
        memory_root = self.composer_state_home / "ssmcp" / "context-memory"
        namespaces = (legacy_root, memory_root)
        self.check(not any(path.is_file() for root in namespaces for path in root.rglob("*")),
                   "normal composer activity creates no automatic conversation or checkpoint files")
        workspace = str(self.workspace.resolve())
        workspace_id = hashlib.sha256(workspace.encode("utf-8")).hexdigest()
        memory_workspace = memory_root / workspace_id
        for path in (legacy_root.parent, legacy_root, memory_root, memory_workspace):
            path.mkdir(mode=0o700, exist_ok=True)
        legacy_id = "conversation_" + "a" * 32
        legacy_message_id = "message_" + "b" * 32
        legacy_branch_id = "context_" + "c" * 32
        legacy_text = "COMPOSER_LEGACY_CONVERSATION_IGNORED"
        checkpoint_text = "COMPOSER_LEGACY_CHECKPOINT_IGNORED"
        legacy_markers = (legacy_text, checkpoint_text, legacy_id)
        legacy_rows = [
            {"type": "session", "schema": "ssmcp.conversation.v1", "schema_version": 1,
             "id": legacy_id, "workspace": workspace, "created_at_ms": 1,
             "truncated_messages": 0},
            {"type": "message", "id": legacy_message_id, "role": "assistant",
             "kind": "agent_message", "content": legacy_text, "created_at_ms": 2,
             "truncated": False},
        ]
        # Match the retired context_memory.rs Document/Branch/Checkpoint schema
        # and canonical workspace hash. All source IDs and bytes are synthetic.
        checkpoint = {
            "schema": "ssmcp.context-memory.v1", "schema_version": 1,
            "workspace": workspace, "workspace_id": workspace_id, "revision": 1,
            "active_id": legacy_branch_id, "branches": [{
                "id": legacy_branch_id, "name": "main", "parent_id": None, "revision": 1,
                "created_at_ms": 1, "updated_at_ms": 2,
                "checkpoint": {"mode": "provided_summary", "text": checkpoint_text,
                               "created_at_ms": 2, "sources": [{
                                   "conversation_id": legacy_id, "message_id": legacy_message_id,
                                   "sha256": hashlib.sha256(legacy_text.encode("utf-8")).hexdigest(),
                               }], "omitted_sources": 0},
                "recent": [], "omitted_recent": 0,
            }],
        }
        legacy_files = {
            legacy_root / (legacy_id + ".jsonl"):
                ("\n".join(json.dumps(row) for row in legacy_rows) + "\n").encode("utf-8"),
            memory_workspace / "state.json": (json.dumps(checkpoint) + "\n").encode("utf-8"),
            # Match the retired archive::Index schema without unknown fields.
            legacy_root / "archives.json": (json.dumps({
                "schema_version": 1, "conversations": [legacy_id],
            }) + "\n").encode("utf-8"),
        }
        for path, payload in legacy_files.items():
            with path.open("xb") as stream:
                os.fchmod(stream.fileno(), 0o600)
                stream.write(payload)

        def legacy_unchanged():
            actual = {path for root in namespaces for path in root.rglob("*") if path.is_file()}
            return actual == set(legacy_files) and all(
                not path.is_symlink() and path.read_bytes() == payload
                and path.stat().st_mode & 0o777 == 0o600
                for path, payload in legacy_files.items()
            )

        live_marker = "COMPOSER_LIVE_BEFORE_RELOAD"
        live_receipt = self.observe("ssmcp.ui.message", {"text": live_marker})
        events_before = self.live_events(limit=4)
        self.check(live_receipt.get("recorded") is True
                   and live_receipt.get("storage") == "memory"
                   and sum(event.get("value", {}).get("message_id") == live_receipt["message_id"]
                           for event in events_before["events"]) == 1,
                   "one real live message is retained before the reload boundary")

        # Leave the next-caller requirement pending after all native work and
        # event-artifact reads. Until /reload, use only UI observation and keys.
        pending = self.submit(front, "COMPOSER_PENDING_MUST_NOT_REPLAY")
        self.focus_input(front)
        front.send(parked.encode() + LEFT * 2)
        front.until(lambda shot: parked in input_text(shot), "the unsent draft before parking")
        parked_cursor = self.queue()["draft"]["cursor"]
        # R copies delivered history into a fresh draft while parking this
        # unrelated text. Same-process Escape restores it; reexec clears RAM.
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
                   and escaped["draft_context"]["target"] == "next",
                   "Escape restores the parked request, recipient, and UTF-8 cursor")
        self.select_request(front, first["id"])
        front.send(b"r")
        front.until(lambda shot: first_text in input_text(shot), "a fresh copy before the reload command")
        before_reload = self.queue()
        before_reload_state = self.ui("/display/conversation")
        self.check(before_reload_state["editing"] is None
                   and before_reload["draft"]["text"] == first_text
                   and before_reload["draft_context"]["parked"]["text"] == parked,
                   "reload starts with a copied delivered message and parked draft, without a held queue edit")
        waiting = [item for item in before_reload["queue"]
                   if item["status"] in ("queued", "sending", "editing")]
        self.check(len(waiting) == 1 and waiting[0]["id"] == pending["id"]
                   and waiting[0]["text"] == pending["text"]
                   and waiting[0]["target"] == "next"
                   and waiting[0]["status"] == "queued"
                   and waiting[0]["room_id"] == self.client_id,
                   "the exact next-caller requirement is still unclaimed immediately before reload")
        old_request_ids = {item["id"] for item in before_reload["queue"]}
        expected_before = [
            {"observed_target_client_id": self.client_id, "message_id": record["id"],
             "text": body, "agent_id": recipient}
            for record, body, recipient in (
                (first, first_text, None), (second, revised, None),
                (third, revised_third, None), (exact, exact["text"], members[0]["agent_id"]),
                (retargeted, retargeted_text, None),
            )
        ]
        self.check(self.gateway_deliveries == expected_before,
                   "every pre-reload code_mode response delivered only the five expected requests")
        old_id, old_token, old_pid = self.client_id, self.secret, front.process.pid
        front.send(b"\x01\x0b/reload\r")
        credentials = self.new_credentials(front)
        self.check(front.process.pid == old_pid and credentials["client_id"] != old_id,
                   "local /reload reexecs the same process and reconnects under a new session ID")
        front.credentials = credentials
        front.client_id = self.client_id = credentials["client_id"]
        front.until(empty_composer_ready, "the fresh empty composer after real reexec")
        # A device reconnect replaces its client ID, while the user's independent
        # account authorization remains valid for the newly registered device.
        status, _, _ = self.rpc("ping", modern=True, secret=old_token)
        self.check(status == 200 and self.secret == old_token,
                   "reexec preserves the independently authorized account OAuth grant")
        def replacement_visible():
            listed = self.public_call("list", secret=old_token)["clients"]
            return (any(item["id"] == self.client_id for item in listed)
                    and all(item["id"] != old_id for item in listed))

        wait_for(replacement_visible, "the same account grant seeing only the replacement client")
        self.check(True, "the same account grant sees the replacement without the disconnected client ID")
        fresh_queue = self.queue()
        fresh_state = self.ui("/display/conversation")
        self.check(fresh_queue["queue"] == []
                   and fresh_queue["draft"] == {"text": "", "cursor": 0}
                   and fresh_queue["draft_context"]["parked"] is None
                   and fresh_queue["draft_context"]["target"] is None
                   and fresh_state["editing"] is None
                   and {key: fresh_state[key] for key in initial_recipient} == initial_recipient,
                   "reexec clears delivered and pending records plus visible and parked drafts")
        self.check(fresh_state["canvas"]["columns"] == default_columns
                   and fresh_state["canvas"]["active"] == 1,
                   "reexec also resets the explicitly expanded canvas to the seven-column conversation default")

        forbidden_after = {live_marker, parked, legacy_message_id, legacy_branch_id,
                           *legacy_markers, *old_request_ids}
        for row in before_reload["queue"]:
            old_body = row.get("text")
            if isinstance(old_body, str) and old_body:
                forbidden_after.add(old_body)
        for event in events_before["events"]:
            old_value = event.get("value", {})
            if old_value.get("type") == "ssmcp.message":
                for field in ("text", "message_id"):
                    old_marker = old_value.get(field)
                    if isinstance(old_marker, str) and old_marker:
                        # Journal event_N identifiers are local to a generation;
                        # their reuse alone is not evidence of restored history.
                        if field == "message_id" and re.fullmatch(r"event_\d+", old_marker):
                            continue
                        forbidden_after.add(old_marker)

        def post_reload_work(**options):
            response = self.work(**options)
            encoded = json.dumps(response, ensure_ascii=False)
            self.check(not any(marker in encoded for marker in forbidden_after),
                       "the actual gateway response contains no discarded RAM or legacy checkpoint text")
            return response

        for attempt in range(3):
            response = post_reload_work()
            self.check(not self.messages(response),
                       f"post-reload gateway response {attempt + 1} replays no old requirement")
            front.pump(0.05)
            self.check(self.queue()["queue"] == [],
                       "actual gateway delivery boundaries leave the fresh queue empty")
        events_after = self.live_events(limit=4)
        serialized_after = json.dumps(events_after["events"], ensure_ascii=False)
        self.check(events_after["generation"] != events_before["generation"]
                   and events_after["scope"]["client_id"] == self.client_id
                   and events_before["scope"]["client_id"] == old_id
                   and not any(event.get("value", {}).get("message_id") == live_receipt["message_id"]
                               and event["value"].get("text") == live_marker
                               for event in events_after["events"])
                   and all(marker not in serialized_after for marker in forbidden_after),
                   "real reexec starts a new event generation without old messages, pending input or draft text")
        self.check(self.gateway_deliveries == expected_before,
                   "all post-reload observations and work responses leave the old pending request undelivered")
        self.check(legacy_unchanged(),
                   "reexec leaves the synthetic legacy JSONL, checkpoint and archive index bytes unchanged")

        # Register fresh authority for the replacement client. A former member
        # identity or a matching display name cannot select this recipient.
        new_members = []
        for role in ("recipient", "other"):
            member = self.public_call("collab", {
                "action": "join", "client_id": self.client_id,
                "name": "Reload member", "role": role,
            })
            self.sensitive.append(member["token"])
            new_members.append(member)
        expected_ids = {member["agent_id"] for member in new_members}
        wait_for(lambda: expected_ids <= {member["agent_id"] for member in self.queue()["agents"]},
                 "the fresh authenticated recipients in the replacement client roster")
        self.recipient(front, new_members[0])
        fresh_text = "COMPOSER_NEW_PROCESS_EXACT_RECIPIENT 界"
        fresh = self.submit(front, fresh_text)
        self.check(fresh["id"] not in old_request_ids
                   and fresh["room_id"] == self.client_id
                   and fresh["target"] == new_members[0]["agent_id"],
                   "fresh input has a new identity and the exact new-client recipient")
        self.ui("/display/conversation/draft")
        observed = self.record(self.queue(), message_id=fresh["id"])
        self.check(observed["status"] == "queued" and observed["text"] == fresh_text
                   and observed["target"] == new_members[0]["agent_id"],
                   "UI-only observation keeps fresh exact-recipient input pending")
        self.check(not self.messages(post_reload_work(controller={"name": "Reload member"})),
                   "a matching post-reload display name cannot claim the new exact recipient")
        self.check(not self.messages(post_reload_work(member=new_members[1])),
                   "another fresh authenticated member cannot claim the exact-recipient input")
        self.expect_delivery(post_reload_work(member=new_members[0]), fresh,
                             request_text=fresh_text, agent_id=new_members[0]["agent_id"])
        self.wait_delivered(front, fresh)
        self.check(not self.messages(post_reload_work(member=new_members[0]))
                   and not self.messages(post_reload_work()),
                   "later gateway responses duplicate neither fresh input nor the discarded pending requirement")
        final_queue = self.queue()
        self.check(len(final_queue["queue"]) == 1
                   and final_queue["queue"][0]["id"] == fresh["id"]
                   and final_queue["queue"][0]["status"] == "delivered"
                   and final_queue["draft"] == {"text": "", "cursor": 0}
                   and final_queue["draft_context"]["parked"] is None,
                   "only the newly delivered request belongs to the fresh process queue")
        self.check_post_delivery_journal(events_after, forbidden_after,
                                         marker="COMPOSER_POST_DELIVERY_JOURNAL_PROBE", limit=4)
        continued_state = self.ui("/display/conversation")
        continued_screen = front.until(
            lambda shot: visible_record(shot, fresh_text, "delivered", fresh["id"])
            and all(marker not in text(shot) for marker in forbidden_after),
            "the newly delivered request without old draft or message bodies",
        )
        continued_ui = json.dumps(continued_state, ensure_ascii=False)
        self.check(continued_state["draft"] == {"text": "", "cursor": 0}
                   and continued_state["editing"] is None
                   and all(marker not in continued_ui and marker not in text(continued_screen)
                           for marker in forbidden_after),
                   "the current UI still excludes every old draft and message after fresh delivery and late probes")
        expected_after = [*expected_before, {
            "observed_target_client_id": self.client_id, "message_id": fresh["id"],
            "text": fresh_text, "agent_id": new_members[0]["agent_id"],
        }]
        self.check(self.gateway_deliveries == expected_after,
                   "the complete HTTP delivery record contains exactly the five old deliveries and one fresh exact-recipient delivery")
        self.check(legacy_unchanged(),
                   "new live work neither modifies nor deletes synthetic files in the retired state namespaces")
        self.check(not (self.workspace / "composer-hidden-shell-leak").exists(),
                   "all composer and reload interactions leave the hidden shell untouched")
        final, restored_termios = front.disconnect()
        self.check(not final["alternate_screen"] and restored_termios == front.original_termios,
                   "normal exit restores the owned terminal after a real client reexec")
        self.check(front.process.returncode == 0, "the reexecuted client disconnects successfully")
        self.metrics["composer"] = {"bytes": len(front.transcript),
                                    "sha256": hashlib.sha256(front.transcript).hexdigest(),
                                    "request_count": len(final_queue["queue"]),
                                    "client_id_before": old_id,
                                    "client_id_after": self.client_id,
                                    "generation_before": events_before["generation"],
                                    "generation_after": events_after["generation"],
                                    "discarded_pending_id": pending["id"],
                                    "gateway_deliveries": list(self.gateway_deliveries),
                                    "legacy_files_sha256": {
                                        str(path.relative_to(self.composer_state_home)):
                                            hashlib.sha256(payload).hexdigest()
                                        for path, payload in legacy_files.items()
                                    }}
        front.close()


def main():
    def interrupted(signum, _frame):
        raise TimeoutError(f"composer E2E stopped by {signal.Signals(signum).name}; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    signal.signal(signal.SIGALRM, interrupted)
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
              "scope": "real composer, queue revisions, same-process drafts, exact recipients, and RAM reset on local reexec; no models",
              "binaries": {name: sha256(path) for name, path in (
                  ("ssmcp", args.bin_dir / "ssmcp"), ("ssmcpd", args.bin_dir / "ssmcpd"), ("vt_snapshot", decoder))},
              "sources": {path.name: sha256(path) for path in (
                  Path(__file__), Path(__file__).with_name("e2e.py"),
                  Path(__file__).with_name("e2e_dashboard.py"),
                  Path(__file__).with_name("e2e_client_ui.py"))}}
    with tempfile.TemporaryDirectory(prefix="ssmcp-composer-e2e-") as directory:
        harness = Composer(Path(directory), args.bin_dir, decoder)

        def redact(description):
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    description = description.replace(secret, "[redacted]")
            return description

        try:
            signal.setitimer(signal.ITIMER_REAL, COMPOSER_TIMEOUT_SECONDS)
            harness.run()
            report["status"] = "passed"
        except Exception as error:
            report["error"] = redact(f"{type(error).__name__}: {error}")
            report["error_traceback"] = redact("".join(traceback.format_tb(error.__traceback__)))
            print(report["error_traceback"] + report["error"], flush=True)
            harness.diagnostics()
        finally:
            signal.setitimer(signal.ITIMER_REAL, 0)
            for cleanup in (harness.close_frontends, harness.cleanup):
                try:
                    cleanup()
                except Exception as error:
                    report["status"] = "failed"
                    message = redact(f"{cleanup.__name__}: {type(error).__name__}: {error}")
                    report.setdefault("cleanup_errors", []).append(message)
                    print(message, flush=True)
            router = harness.account_gateway
            report["router"] = {"lifetime_seconds": harness.fixture_timeout_seconds,
                                "exit_code": router.exit_code}
            if router.prepared and router.exit_code != 0:
                with (harness.directory / "account-router.log").open("rb") as log:
                    log.seek(max(0, os.fstat(log.fileno()).st_size - 4096))
                    tail = redact(log.read().decode(errors="replace"))
                report["router"]["log_tail"] = tail
                print(f"Account router exit {router.exit_code}:\n{tail}", flush=True)
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
