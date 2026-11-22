#!/usr/bin/env python3
"""Verify caller grouping, member selection and exact delivery in an owned PTY.

Build ssmcp, ssmcpd and examples/vt_snapshot, then run:
  python3 scripts/e2e_agent_panel.py --bin-dir target/debug --out /tmp/agent-panel

Reuses the composer fixture's isolated account/OAuth connection and real terminal
decoder. The two members are test identities; no model or live session is used.
Snapshots and PNGs come from the production compositor, using the existing
render_ui_snapshot.py renderer (Pillow and a monospace font are required).
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import signal
import subprocess
import sys
import tempfile
import time

from e2e_composer import Composer, DOWN, ENTER, ESC, HOME
from e2e_client_ui import column, empty_composer_ready
from e2e_dashboard import sha256, text


CALLER = {"name": "ChatGPT", "model": "Model A"}
OBSERVER = {"name": "Panel observer", "model": "Fixture observer"}


def list_rows(snapshot):
    # Match the production Sessions list width, excluding the details/sidebar.
    # Verified account attribution precedes the caller's display name.
    columns = snapshot["size"][1]
    divider = snapshot["rows"][0].find("│")
    width = divider if divider >= 0 else min(2 * columns // 5, 40) if columns >= 76 else columns
    return [row[:width] for row in snapshot["rows"][1:-5]]


def selected_name(snapshot, name):
    return any("›" in row and name in row for row in list_rows(snapshot))


class AgentPanel(Composer):
    def __init__(self, directory, binary_dir, decoder, output, font):
        super().__init__(directory, binary_dir, decoder)
        self.output = output
        self.font = font
        self.snapshots = []
        self.event_artifacts = []

    def config(self, name, **_options):
        path = self.directory / f"{name}.toml"
        path.write_text(
            '[ui]\ndashboard=true\ncolor=true\nlog_format="pretty"\n'
            'timestamps=false\nlayout="chat"\nsidebar=false\nsidebar_width=32\n'
            'panels=["agents", "queue"]\n',
            encoding="utf-8",
        )
        return path

    def response(self, code, *, agent=None, controller=None):
        # Observation calls remain visible under their own stable display group.
        # They never masquerade as a member or the caller under test.
        return super().response(
            code, agent=agent, controller=OBSERVER if controller is None else controller
        )

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

    def save_shot(self, name, front):
        self.output.mkdir(parents=True, exist_ok=True)
        source = self.output / f"agent-panel-{name}.json"
        rendered = source.with_suffix(".png")
        source.write_text(
            json.dumps(front.snapshot(cells=True), ensure_ascii=False, indent=2) + "\n",
            encoding="utf-8",
        )
        subprocess.run(
            [sys.executable, str(Path(__file__).with_name("render_ui_snapshot.py")),
             str(source), str(rendered), "--font", str(self.font), "--size", "16"],
            check=True, timeout=15,
        )
        self.snapshots.append({"cells": source.name, "image": rendered.name})

    def select_name(self, front, name):
        front.send(HOME)
        front.until(lambda shot: selected_name(shot, "Next caller"), "the first roster entry")
        for _ in range(12):
            shot = front.snapshot()
            if selected_name(shot, name):
                return shot
            front.send(DOWN)
            front.pump(0.08)
        raise AssertionError(f"keyboard navigation did not reach {name}")

    def choose_member(self, front, member):
        self.agents(front)
        front.until(
            lambda shot: "…" + member["agent_id"][-8:] in "\n".join(list_rows(shot)),
            "the authenticated member arriving in the asynchronous roster",
        )
        roster = self.queue()["agents"]
        index = 1 + next(index for index, value in enumerate(roster)
                         if value["agent_id"] == member["agent_id"])
        front.send(HOME + DOWN * index + ENTER)
        front.until(lambda shot: (column(shot) or ())[:2] == ("conversation", 2) and not shot["hide_cursor"]
                    and "To Builder" in text(shot), "the selected member's composer")
        state = self.ui("/display/conversation")
        self.check(state["target"] == member["agent_id"],
                   "keyboard selection targets the exact member ID despite equal display names")


    def click_sidebar(self, front, label, *, occurrence=0, expected=1, button=0):
        def hits(shot):
            cells = shot["cells"]
            divider = next((i for i, cell in enumerate(cells[0])
                            if cell["text"] == "│"), None)
            if divider is None:
                return []
            found = []
            for y, row in enumerate(cells[:-1]):
                for x in range(divider + 1, len(row) - len(label) + 1):
                    if "".join(cell["text"] for cell in row[x:x + len(label)]) == label:
                        found.append((x, y))
            return found
        shot = front.until(lambda shot: len(hits(shot)) == expected,
                           "visible sidebar rows for " + label, cells=True)
        x, y = hits(shot)[occurrence]
        sequence = f"\x1b[<{button};{x + 1};{y + 1}"
        front.send((sequence + "M" + sequence + "m").encode())

    def marker(self, text, member=None):
        call = "return await call(%s, 'ssmcp.ui.message', %s);" % (
            json.dumps(self.client_id), json.dumps({"text": text, "kind": "progress"}))
        if member is not None:
            call = "await collab.bind(%s, %s); %s" % (
                json.dumps(self.client_id), json.dumps({
                    "agent_id": member["agent_id"], "token": member["token"]}), call)
        result = self.response(call)
        native = result["structuredContent"]["result"]
        receipt = native.get("structuredContent", {})
        self.check(not native.get("isError") and receipt.get("accepted") is True
                   and receipt.get("recorded") is True and receipt.get("storage") == "memory" and receipt.get("shown") is True,
                   "owned thread marker is recorded in current process memory")
        return receipt["message_id"]

    def event_result(self, name, arguments):
        if name not in ("ssmcp.ui.events", "ssmcp.output.read"):
            raise AssertionError("unexpected event observation tool")
        result = self.response("return await call(%s, %s, %s);" % (
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments)))
        if self.messages(result):
            raise AssertionError("event observation consumed a user requirement")
        native = result["structuredContent"]["result"]
        if native.get("isError"):
            raise AssertionError(f"native event observation failed: {native}")
        return native

    def read_event_artifact(self, summary):
        # The production transport may spill a valid large process page.
        # Use the same authenticated output reader as the references fixture.
        meta = summary.get("artifact", {})
        size = meta.get("bytes")
        if (summary.get("format") != "mcp_tool_result_json" or summary.get("output_error")
                or meta.get("schema") != "ssmcp.output.v1"
                or meta.get("scope") != "native_tool_result_json"
                or meta.get("storage") != "memory"
                or meta.get("complete") is not True
                or not isinstance(size, int) or not 0 < size <= 1024 * 1024
                or meta.get("observed_bytes") != size
                or meta.get("sha256_scope") != "first_bytes_snapshot"
                or not re.fullmatch(r"[0-9a-f]{32}", str(meta.get("id", "")))
                or not re.fullmatch(r"[0-9a-f]{64}", str(meta.get("sha256", "")))):
            raise AssertionError("event spill is not a complete bounded native JSON artifact")
        # output.read is eligible work; never let observation take a real request.
        if any(item["status"] != "delivered" for item in self.queue()["queue"]):
            raise AssertionError("event artifact reads require an entirely delivered queue")
        payload = bytearray()
        chunks = 0
        while len(payload) < size:
            offset = len(payload)
            part = self.event_result("ssmcp.output.read", {
                "id": meta["id"], "offset": offset,
                "max_bytes": min(8192, size - offset)})["structuredContent"]
            chunk = base64.b64decode(part["data_base64"], validate=True)
            end = offset + len(chunk)
            if (not chunk or len(chunk) > 8192 or end > size
                    or part.get("id") != meta["id"] or part.get("offset") != offset
                    or part.get("next_offset") != end or part.get("bytes") != len(chunk)
                    or part.get("size") != size or part.get("complete") is not True
                    or part.get("eof") is not (end == size)
                    or part.get("sha256_scope") != "returned_chunk"
                    or part.get("sha256") != hashlib.sha256(chunk).hexdigest()):
                raise AssertionError("output.read returned an invalid event chunk")
            payload.extend(chunk)
            chunks += 1
        if hashlib.sha256(payload).hexdigest() != meta["sha256"]:
            raise AssertionError("complete event artifact digest differs from its manifest")
        native = json.loads(payload)
        if (not isinstance(native, dict) or native.get("isError")
                or not isinstance(native.get("structuredContent"), dict)):
            raise AssertionError("event artifact is not a successful native MCP result")
        self.event_artifacts.append({
            "id": meta["id"], "bytes": size, "sha256": meta["sha256"],
            "complete": True, "chunks": chunks})
        return native

    def thread_page(self, member=None, *, main=False, cursor=None, generation=None, limit=64):
        arguments = {"action": "thread", "client_id": self.client_id, "limit": limit}
        if cursor is not None:
            arguments.update(cursor=cursor, generation=generation)
        if member is not None:
            arguments["agent_id"] = member["agent_id"]
        elif main:
            arguments["main"] = True
        native = self.event_result("ssmcp.ui.events", arguments)
        data = native.get("structuredContent", {})
        if data.get("spilled"):
            data = self.read_event_artifact(data)["structuredContent"]
        if (data.get("schema_version") != 1 or not isinstance(data.get("events"), list)
                or data.get("scope", {}).get("client_id") != self.client_id
                or data.get("retention") != "live_process_memory"):
            raise AssertionError("complete native result did not contain the live event schema")
        return data

    @staticmethod
    def message_texts(page):
        return {event["value"]["text"] for event in page["events"]
                if event.get("value", {}).get("type") == "ssmcp.message"}

    def events_containing_messages(self, wanted, *, main=False):
        # A page is not the whole journal. Follow the real cursor/generation,
        # stopping at the known live markers instead of chasing new observer calls.
        wanted = set(wanted)
        events = []
        seen = set()
        cursor = generation = scope = None
        for _ in range(32):
            page = self.thread_page(main=main, cursor=cursor,
                                    generation=generation, limit=16)
            if generation is not None and (page["generation"] != generation
                                           or page["scope"] != scope):
                raise AssertionError("event paging changed the owner or process generation")
            if page.get("retention_gap_possible"):
                raise AssertionError("owned fixture lost live events before its marker assertions")
            generation, scope = page["generation"], page["scope"]
            for event in page["events"]:
                if event["sequence"] in seen:
                    raise AssertionError("event cursor repeated an event")
                seen.add(event["sequence"])
                events.append(event)
            combined = {**page, "events": events}
            if wanted <= self.message_texts(combined):
                return combined
            next_cursor = page["next_cursor"]
            if not page["has_more"] or (cursor is not None and next_cursor <= cursor):
                raise AssertionError("retained event markers were not found before the journal ended")
            cursor = next_cursor
        raise AssertionError("owned event marker search exceeded its bounded page count")

    def leave_thread_events(self, front):
        front.send(ESC)
        front.until(lambda shot: (column(shot) or ())[:2] == ("sessions", 1),
                    "live events return to Sessions")
        front.send(ESC)
        front.until(lambda shot: (column(shot) or ())[:2] == ("conversation", 2)
                    and not shot["hide_cursor"], "Sessions returns to the same composer")

    def member_event_actions(self, front, members, composition):
        # Exercise the site's default sidebar=false layout first.
        front.send(b"\x1bOR")
        front.until(lambda shot: "│" not in shot["rows"][0],
                    "the member sidebar is explicitly hidden")
        self.check(self.ui("/config")["sidebar"] is False,
                   "keyboard event viewing works without the optional sidebar")
        queue_before = self.queue()["queue"]
        first, second = members
        markers = ("AGENT_PANEL_A_ONLY", "AGENT_PANEL_B_ONLY", "AGENT_PANEL_MAIN_ONLY")
        self.marker(markers[0], first)
        self.marker(markers[1], second)
        self.marker(markers[2])
        current = self.thread_page()
        self.check(current["scope"]["client_id"] == self.client_id,
                   "both same-name members belong to this live client process")

        for member, own, other in ((first, markers[0], markers[1]),
                                   (second, markers[1], markers[0])):
            page = self.thread_page(member)
            serialized = json.dumps(page["events"], ensure_ascii=False)
            self.check(page["scope"]["agent_id"] == member["agent_id"]
                       and page["scope"]["client_id"] == self.client_id
                       and page["scope"]["provenance"] == "authenticated_call_scope"
                       and page["scope"]["includes_unattributed"] is False
                       and own in serialized and other not in serialized
                       and markers[2] not in serialized
                       and all(event["source"]["agent_id"] == member["agent_id"]
                               for event in page["events"]),
                       "same-name thread events contains only its authenticated marker")
            self.agents(front)
            self.highlight_recipient(front, member)
            front.send(b"h")
            state = self.wait_conversation(front, lambda state:
                state["view"] == "live_logs" and state["thread_history"] is not None
                and state["thread_history"]["scope"] == page["scope"],
                "h opens the exact selected member events")
            shown = front.until(lambda shot: own in text(shot), "the selected thread marker is visible")
            self.check(other not in text(shown) and markers[2] not in text(shown)
                       and state["thread_history"]["read_only"] is True
                       and {key: state[key] for key in composition} == composition,
                       "the live event frame excludes the other member and keeps the composition")
            if member is second:
                self.marker("AGENT_PANEL_A_LATE", first)
                late = self.thread_page(first)
                state = self.wait_conversation(front, lambda state:
                    state["thread_history"] is not None
                    and state["thread_history"]["scope"] == page["scope"]
                    and state["thread_history"]["next_cursor"] >= late["next_cursor"],
                    "the B event view has scanned past the late A event")
                shown = front.snapshot()
                self.check("AGENT_PANEL_A_LATE" not in text(shown)
                           and {key: state[key] for key in composition} == composition,
                           "a late A reply never appears in B's already open event view")
            self.leave_thread_events(front)

        self.agents(front)
        self.highlight_recipient(front, first)
        front.send(b"h")
        front.until(lambda shot: "AGENT_PANEL_A_LATE" in text(shot),
                    "returning to A reveals its late reply")
        self.leave_thread_events(front)
        aggregate = self.events_containing_messages((*markers, "AGENT_PANEL_A_LATE"))
        main = self.events_containing_messages((markers[2],), main=True)
        self.check(set((*markers, "AGENT_PANEL_A_LATE")) <= self.message_texts(aggregate)
                   and markers[2] in self.message_texts(main)
                   and markers[0] not in self.message_texts(main)
                   and markers[1] not in self.message_texts(main),
                   "the process overview retains both members and main remains a separate scope")
        returned = self.ui("/display/conversation")
        owner_after = self.thread_page()
        self.check(returned["view"] == "activity" and returned["thread_history"] is None
                   and {key: returned[key] for key in composition} == composition
                   and owner_after["scope"] == current["scope"]
                   and owner_after["generation"] == current["generation"]
                   and self.queue()["queue"] == queue_before,
                   "event navigation preserves owner, draft, recipients, group and queue")

        front.send(b"\x1bOR")
        roster = self.queue()["agents"]
        chosen = next(member for member in members if member["agent_id"] == roster[0]["agent_id"])
        page = self.thread_page(chosen)
        self.click_sidebar(front, "Builder", expected=2)
        picked = self.wait_conversation(front, lambda state:
            state["view"] == "agents" and state["selected_agent"] == "member:" + chosen["agent_id"],
            "sidebar highlights the exact painted member")
        self.check({key: picked[key] for key in composition} == composition
                   and self.queue()["queue"] == queue_before,
                   "sidebar left-click selects a row without sending or changing recipients")
        front.send(ESC)
        front.until(lambda shot: (column(shot) or ())[:2] == ("conversation", 2), "return to sidebar overview")
        self.click_sidebar(front, "Builder", expected=2, button=2)
        opened = self.wait_conversation(front, lambda state:
            state["view"] == "live_logs" and state["thread_history"] is not None
            and state["thread_history"]["scope"] == page["scope"],
            "sidebar right-click opens the exact member without prior selection")
        self.check(opened["thread_history"]["read_only"] is True
                   and {key: opened[key] for key in composition} == composition
                   and self.queue()["queue"] == queue_before,
                   "sidebar event view keeps the original composition and never sends")
        self.leave_thread_events(front)

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(
            ["git", "-c", "init.defaultBranch=main", "init", "-q"],
            cwd=self.workspace, env={**os.environ, **self.safe_git_env},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5,
        )
        front = self.frontend("agent-panel", self.config("agent-panel"))
        self.oauth_login(front.credentials)
        front.resize(30, 160)
        front.until(empty_composer_ready, "the isolated conversation composer")
        front.send(b"\x1bOR")
        front.until(lambda shot: "│" in text(shot) and "Members" in text(shot),
                    "the explicitly opened member sidebar")

        for _ in range(3):
            self.work(controller=CALLER)
        self.agents(front)
        shown = self.select_name(front, CALLER["name"])
        self.check(sum(CALLER["name"] in row for row in list_rows(shown)) == 1,
                   "three completed calls produce one caller row in the agent list")
        self.check(not self.queue()["agents"],
                   "observed callers do not fabricate authenticated session members")
        self.save_shot("callers", front)
        front.send(ESC)
        front.until(lambda shot: (column(shot) or ())[:2] == ("conversation", 2) and not shot["hide_cursor"], "the original composer")

        members = []
        for role in ("Implementation", "Review"):
            member = self.public_call("collab", {
                "action": "join", "client_id": self.client_id,
                "name": "Builder", "role": role,
            })
            self.sensitive.append(member["token"])
            members.append(member)
        self.choose_member(front, members[0])
        self.choose_member(front, members[1])
        draft = "Preserve this draft 界"
        front.send(draft.encode("utf-8") + b"\x01")
        original = self.wait_conversation(
            front, lambda state: state["draft"]["text"] == draft and state["draft"]["cursor"] == 0,
            "the Unicode draft did not retain its cursor at the beginning",
        )
        composition = {key: original[key] for key in
                       ("draft", "target", "editing", "native_recipient", "group_selected")}
        self.member_event_actions(front, members, composition)
        self.agents(front)
        shown = self.select_name(front, CALLER["name"])
        self.check(sum("Builder" in row and "↳" not in row for row in list_rows(shown)) == 2,
                   "two same-name members retain separate visible roster entries")
        self.check(all("…" + member["agent_id"][-8:] in "\n".join(list_rows(shown))
                       for member in members),
                   "same-name member rows expose their distinct identity suffixes")
        selected = self.ui("/display/conversation")["selected_agent"]
        self.work(controller=CALLER)
        front.until(lambda shot: selected_name(shot, CALLER["name"]),
                    "the same selected caller after its next request")
        updated = self.ui("/display/conversation")
        self.check(updated["selected_agent"] == selected,
                   "a new request preserves the selected caller's stable identity")
        self.check({key: updated[key] for key in composition} == composition,
                   "caller navigation preserves draft, cursor, exact recipient and edit state")
        shown = front.snapshot()
        self.check(sum(CALLER["name"] in row for row in list_rows(shown)) == 1,
                   "a fourth call updates the existing caller without adding another row")
        self.save_shot("wide", front)

        front.send(ENTER)
        front.until(lambda shot: "Observed call ·" in text(shot)
                    and "terminal.status" in text(shot),
                    "the selected caller's actual retained tool invocation")
        details = self.ui("/display/conversation")
        self.check(details["view"] == "agents" and details["focus"] == "content"
                   and details["target"] == members[1]["agent_id"],
                   "opening a caller's audit details leaves the authenticated recipient unchanged")
        self.check(details["draft"] == composition["draft"],
                   "audit details preserve the existing Unicode draft and cursor")
        self.save_shot("call-details", front)
        expected_run = self.ui("/display/audit/selected/run_id")
        front.send(ESC)
        front.until(lambda shot: selected_name(shot, CALLER["name"]), "back to the selected caller")
        front.send(ESC)
        front.until(lambda shot: (column(shot) or ())[:2] == ("conversation", 2), "the caller sidebar")
        self.click_sidebar(front, CALLER["model"], button=2)
        front.until(lambda shot: "Observed call ·" in text(shot)
                    and "terminal.status" in text(shot), "sidebar opens the drawn call audit")
        reopened = self.ui("/display/conversation")
        self.check(self.ui("/display/audit/detail") is True
                   and self.ui("/display/audit/selected/run_id") == expected_run
                   and {key: reopened[key] for key in composition} == composition,
                   "observed sidebar event view opens the exact run without changing the recipient")
        front.send(ESC)
        front.until(lambda shot: selected_name(shot, CALLER["name"]),
                    "Escape returning to the same caller row")
        front.resize(18, 70)
        shown = front.until(lambda shot: shot["size"] == [18, 70]
                            and selected_name(shot, CALLER["name"]),
                            "the selected caller in the narrow terminal")
        self.check(sum(CALLER["name"] in row for row in list_rows(shown)) == 1,
                   "narrow terminals retain one selected caller row")
        self.save_shot("narrow", front)
        self.work(controller={"name": CALLER["name"], "model": "Model B"})
        shown = front.until(
            lambda shot: CALLER["model"] in "\n".join(list_rows(shot))
            and "Model B" in "\n".join(list_rows(shot)),
            "distinct models remaining identifiable in the narrow caller list",
        )
        self.check(sum(CALLER["name"] in row for row in list_rows(shown)) == 2,
                   "same-name callers with different models remain distinct and identifiable")
        self.save_shot("narrow-models", front)

        front.send(ESC)
        front.until(lambda shot: (column(shot) or ())[:2] == ("conversation", 2) and not shot["hide_cursor"], "the preserved composer")
        returned = self.ui("/display/conversation")
        self.check({key: returned[key] for key in composition} == composition,
                   "returning from the resized roster preserves the exact original composition")
        front.resize(58, 106)
        front.until(lambda shot: shot["size"] == [58, 106], "the fixture's delivery layout")
        front.send(b"\x01\x0b")
        self.wait_conversation(front, lambda state: not state["draft"]["text"],
                               "clearing the test-owned draft before submitting the delivery check")
        self.command(front, "/queue")
        request_text = "AGENT_PANEL_EXACT_RECIPIENT"
        record = self.submit(front, request_text)
        self.check(record["target"] == members[1]["agent_id"],
                   "the submitted requirement retains the chosen member ID")
        self.check(not self.messages(self.work(member=members[0])),
                   "a different same-name member cannot receive the directed requirement")
        self.check(not self.messages(self.work(controller=CALLER)),
                   "an observed caller cannot receive the directed requirement")
        self.expect_delivery(self.work(member=members[1]), record, agent_id=members[1]["agent_id"])
        self.check(not self.messages(self.work(member=members[1])),
                   "the exact recipient receives the requirement once")
        self.metrics.update(registered_members=2, repeated_calls=4,
                            snapshot_count=len(self.snapshots))


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("agent panel E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--fixture-binary", type=Path, default=os.environ.get("SSMCP_E2E_FIXTURE_BINARY"),
                        help="prebuilt library fixture; overrides SSMCP_E2E_FIXTURE_BINARY")
    parser.add_argument("--decoder", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--report", type=Path)
    parser.add_argument("--font", type=Path,
                        default=Path("/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf"))
    args = parser.parse_args()
    if args.fixture_binary is not None:
        fixture = args.fixture_binary.resolve()
        if not fixture.is_file() or not os.access(fixture, os.X_OK):
            parser.error(f"prebuilt fixture is not executable: {fixture}")
        os.environ["SSMCP_E2E_FIXTURE_BINARY"] = str(fixture)
    decoder = args.decoder or args.bin_dir / "examples/vt_snapshot"
    for executable in (args.bin_dir / "ssmcp", args.bin_dir / "ssmcpd", decoder):
        if not executable.is_file():
            parser.error(f"missing {executable}; build the binaries and vt_snapshot first")
    if not args.font.is_file():
        parser.error(f"missing monospace font {args.font}; pass --font")
    started = time.monotonic()
    report = {
        "status": "failed", "platform": platform.system(),
        "transport": "isolated account/OAuth + owned PTY + independent vt100 decoder",
        "scope": "caller grouping, keyboard/sidebar live thread events, same-name isolation and exact delivery; no models",
        "binaries": {name: sha256(path) for name, path in (
            ("ssmcp", args.bin_dir / "ssmcp"), ("ssmcpd", args.bin_dir / "ssmcpd"),
            ("vt_snapshot", decoder),
        )},
    }
    if args.fixture_binary is not None:
        report["binaries"]["fixture"] = sha256(args.fixture_binary.resolve())
    with tempfile.TemporaryDirectory(prefix="ssmcp-agent-panel-e2e-") as directory:
        harness = AgentPanel(Path(directory), args.bin_dir, decoder, args.out, args.font)
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
                          snapshots=harness.snapshots,
                          event_artifacts=harness.event_artifacts,
                          duration_seconds=round(time.monotonic() - started, 3))
            destination = args.report or args.out / "agent-panel-result.json"
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n",
                                   encoding="utf-8")
    if report["status"] != "passed":
        raise SystemExit(1)
    print(f"\nPassed {len(report['checks'])} agent panel checks in {report['duration_seconds']}s",
          flush=True)


if __name__ == "__main__":
    main()

