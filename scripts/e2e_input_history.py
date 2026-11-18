#!/usr/bin/env python3
"""Exercise references, image preview, copy/help, and persisted history in owned PTYs.

Build ssmcp, ssmcpd, and examples/vt_snapshot first, then run:
  python3 scripts/e2e_input_history.py --bin-dir target/debug --out /tmp/input-history

Reuses the isolated SSH/OAuth fixture and independent VT decoder. HOME and all
XDG data/config/state directories are temporary; the restarted client shares
only this test's workspace and history. No model or installed harness is used.
"""

import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import re
import shlex
import signal
import struct
import subprocess
import tempfile
import time
import zlib

from e2e_composer import Composer, DOWN, ENTER, ESC, HOME, RIGHT
from e2e_dashboard import sha256, text


F1 = b"\x1bOP"
F7 = b"\x1b[18~"
END = b"\x1b[F"
PASTE_START = b"\x1b[200~"
PASTE_END = b"\x1b[201~"
REFERENCE_BEGIN = "\n\n<ssmcp-references-v1>\n"
REFERENCE_END = "\n</ssmcp-references-v1>"


def png_fixture():
    """A real two-by-two RGB PNG: red upper pixels, blue lower pixels."""
    def chunk(kind, data):
        return (struct.pack(">I", len(data)) + kind + data
                + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))

    pixels = b"\x00" + bytes([255, 0, 0]) * 2 + b"\x00" + bytes([0, 0, 255]) * 2
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", 2, 2, 8, 2, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(pixels)) + chunk(b"IEND", b""))


def mouse_modes(data):
    # The VT decoder exposes pixels/cells but not DEC mouse modes. Inspect the
    # actual output controls, allowing both separate and grouped mode changes.
    modes = {}
    for parameters, action in re.findall(rb"\x1b\[\?([0-9;]+)([hl])", data):
        for mode in parameters.split(b";"):
            modes[int(mode)] = action == b"h"
    return modes


class InputHistory(Composer):
    def __init__(self, directory, binary_dir, decoder, output):
        super().__init__(directory, binary_dir, decoder)
        self.output = output.resolve()
        self.output.mkdir(parents=True, exist_ok=True)
        self.home = directory / "home"
        self.state_home = directory / "state"
        for name in ("home", "state", "config", "data", "cache", "runtime"):
            (directory / name).mkdir(mode=0o700)

        # Frontend deliberately forces NO_COLOR for its other suites. A private
        # launcher changes that one setting for this real RGB-image scenario.
        launchers = directory / "launchers"
        launchers.mkdir(mode=0o700)
        (launchers / "ssmcpd").symlink_to(self.binary_dir / "ssmcpd")
        launcher = launchers / "ssmcp"
        launcher.write_text(
            "#!/bin/sh\nunset NO_COLOR\nexec "
            + shlex.quote(str(self.binary_dir / "ssmcp")) + ' "$@"\n'
        )
        launcher.chmod(0o700)
        self.binary_dir = launchers

    def client_environment(self, label, **options):
        environment = super().client_environment(label, **options)
        environment.update({
            "HOME": str(self.home),
            "XDG_STATE_HOME": str(self.state_home),
            "XDG_CONFIG_HOME": str(self.directory / "config"),
            "XDG_DATA_HOME": str(self.directory / "data"),
            "XDG_CACHE_HOME": str(self.directory / "cache"),
            "XDG_RUNTIME_DIR": str(self.directory / "runtime"),
        })
        return environment

    def config(self, name, **_options):
        path = self.directory / f"{name}.toml"
        path.write_text(
            '[ui]\ndashboard=true\ncolor=true\nlog_format="pretty"\n'
            'timestamps=false\nlayout="chat"\nsidebar=false\n'
        )
        return path

    def save_shot(self, name, front, *, cells=False):
        snapshot = front.snapshot(cells=cells)
        (self.output / f"input-history-{name}.json").write_text(
            json.dumps(snapshot, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        (self.output / f"input-history-{name}.txt").write_text(
            text(snapshot) + "\n", encoding="utf-8"
        )
        return snapshot

    def wait_state(self, front, predicate, description):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            state = self.ui("/display/conversation")
            if predicate(state):
                return state
            if front.process.poll() is not None:
                break
        raise AssertionError(description + "\n" + front.failure_detail())

    def send_stream(self, front, data):
        # The owned PTY is nonblocking. Unlike a single os.write, this preserves
        # every byte of a paste larger than the terminal's input buffer.
        pending = memoryview(data)
        deadline = time.monotonic() + 8
        while pending:
            if time.monotonic() >= deadline:
                raise AssertionError("owned PTY could not accept the complete paste")
            try:
                count = os.write(front.master, pending[:2048])
                pending = pending[count:]
            except BlockingIOError:
                pass
            front.pump(0.01)

    def queue_current(self, front, source):
        before = {item["id"] for item in self.queue()["queue"]}
        self.check(self.ui("/display/conversation")["draft"]["text"] == source,
                   "the exact user draft is present before submission")
        front.send(ENTER)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            created = [item for item in self.queue()["queue"] if item["id"] not in before]
            if len(created) == 1 and created[0]["status"] == "queued":
                self.wait_state(front, lambda state: not state["draft"]["text"],
                                "the accepted draft did not clear")
                return created[0]
        raise AssertionError("the submitted requirement was not acknowledged by the real queue")

    def submit_source(self, front, source):
        self.check(not self.ui("/display/conversation")["draft"]["text"],
                   "the next reference scenario starts with an empty composer")
        front.send(source)
        self.wait_state(front, lambda state: state["draft"]["text"] == source,
                        "the UTF-8 draft did not reach the composer")
        return self.queue_current(front, source)

    def complete_reference(self, front, typed, candidate, expected, key, shot_name):
        before = {item["id"] for item in self.queue()["queue"]}
        front.send(typed)
        front.until(
            lambda shot: any(row.lstrip().startswith("› " + candidate + "  ")
                             for row in shot["rows"]),
            f"the real enumerated {candidate} completion",
        )
        self.save_shot(shot_name, front)
        front.send(key)
        state = self.wait_state(
            front, lambda state: state["draft"]["text"] == expected,
            f"accepting {candidate} did not preserve the surrounding UTF-8 draft",
        )
        label = "Tab" if key == b"\t" else "Enter"
        self.check(state["draft"]["cursor"] == len(expected.encode("utf-8")),
                   f"{label} completes {candidate} at its correct UTF-8 cursor")
        self.check({item["id"] for item in self.queue()["queue"]} == before,
                   f"the first {label} accepts {candidate} without queuing a requirement")
        return expected

    def delivered_references(self, front, member, record, source):
        response = self.work(member=member)
        self.expect_delivery(response, record, agent_id=member["agent_id"])
        message = self.messages(response)[0]["text"]
        original, separator, encoded = message.partition(REFERENCE_BEGIN)
        self.check(separator and original == source and encoded.endswith(REFERENCE_END),
                   "delivery preserves the user's text and includes explicit reference data")
        payload = json.loads(encoded[:-len(REFERENCE_END)])
        self.check(payload["source_sha256"] == hashlib.sha256(source.encode()).hexdigest(),
                   "delivered references identify the exact UTF-8 source")
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            if self.record(self.queue(), message_id=record["id"])["status"] == "delivered":
                return payload["references"]
        raise AssertionError("the local queue did not observe the reference delivery")

    def start_front(self, label):
        front = self.frontend(label, self.config(label))
        self.oauth_login(front.credentials)
        front.until(lambda shot: shot["alternate_screen"] and "To · next caller" in text(shot),
                    "the isolated live composer")
        return front

    def reference_scenarios(self, front, member):
        notes_path = "资料/说明 界.txt"
        notes = "UTF8_FILE_FIXTURE\n中文界 café e\u0301\n"
        (self.workspace / "资料").mkdir()
        (self.workspace / notes_path).write_text(notes, encoding="utf-8")
        skill_path = ".agents/skills/input-fixture/SKILL.md"
        skill = (
            "---\nname: input-fixture\ndescription: Local input fixture with UTF-8 内容.\n---\n\n"
            "# Fixture skill\n\nSKILL_BODY_FIXTURE 界\n"
        )
        (self.workspace / skill_path).parent.mkdir(parents=True)
        (self.workspace / skill_path).write_text(skill, encoding="utf-8")
        image_path = "资料/红蓝.png"
        image_bytes = png_fixture()
        (self.workspace / image_path).write_bytes(image_bytes)
        # Create every file before enumeration so a cached directory listing is
        # a real fixture snapshot, not an accidental cache-expiry dependency.
        file_draft = self.complete_reference(
            front, "REFERENCE_REQUEST 界 @资料/说", "@" + notes_path,
            f'REFERENCE_REQUEST 界 @"{notes_path}" ', b"\t", "complete-file",
        )
        source = self.complete_reference(
            front, "$input-f", "$input-fixture", file_draft + "$input-fixture ",
            ENTER, "complete-skill",
        )
        # The prior Enter only accepted the skill. This second Enter must send
        # exactly this completed draft to the explicitly selected fixture agent.
        record = self.queue_current(front, source)
        references = self.delivered_references(front, member, record, source)
        file_ref = next(item for item in references if item["kind"] == "text")
        skill_ref = next(item for item in references if item["kind"] == "skill")
        self.check(file_ref["path"] == notes_path and file_ref["preview"] == notes,
                   "a quoted UTF-8 file reference delivers its actual preview")
        self.check(skill_ref["name"] == "input-fixture" and skill_ref["root"] == "workspace",
                   "the requested local fixture skill is selected from the workspace")
        file_data = self.call(file_ref["read"]["tool"], file_ref["read"]["arguments"])
        skill_data = self.call(skill_ref["read"]["tool"], skill_ref["read"]["arguments"])
        self.check(file_data["text"] == notes and skill_data["content"] == skill,
                   "delivered file and skill read actions retrieve the complete fixture contents")
        self.save_shot("references", front)

        image_source = self.complete_reference(
            front, "IMAGE_REQUEST @资料/红", "@" + image_path,
            f"IMAGE_REQUEST @{image_path} ", ENTER, "complete-image",
        )
        image_record = self.queue_current(front, image_source)
        image_refs = self.delivered_references(front, member, image_record, image_source)
        self.check(len(image_refs) == 1 and image_refs[0]["kind"] == "image"
                   and image_refs[0]["read"]["tool"] == "ssmcp.fs.read_image",
                   "the PNG reference exposes the native image read action")
        response = self.response(
            "const result = await call(%s, 'ssmcp.fs.read_image', %s); "
            "image(result); return result.structuredContent;" % (
                json.dumps(self.client_id), json.dumps(image_refs[0]["read"]["arguments"]),
            )
        )
        self.check(not self.messages(response), "reading an already delivered image resends no request")
        images = [item for item in response["content"] if item.get("type") == "image"]
        self.check(len(images) == 1 and images[0]["mimeType"] == "image/png"
                   and base64.b64decode(images[0]["data"], validate=True) == image_bytes,
                   "read_image and image() return the complete PNG as actual MCP image content")
        metadata = response["structuredContent"]["result"]
        self.check(metadata["path"] == image_path and metadata["size"] == len(image_bytes)
                   and metadata["sha256"] == hashlib.sha256(image_bytes).hexdigest(),
                   "the image metadata describes the same bytes returned to the model")

        def red_over_blue(shot):
            return any(
                cell["text"] == "▀" and cell["foreground"] == [255, 0, 0]
                and cell["background"] == [0, 0, 255]
                for row in shot.get("cells", []) for cell in row
            )

        front.until(red_over_blue, "red/blue image pixels in real terminal half-block cells", cells=True)
        self.check(True, "the TUI visibly previews the PNG with true-color half-block pixels")
        self.save_shot("image", front, cells=True)
        return source

    def read_text_pages(self, path, expected):
        """Use native UTF-8 cursors; even newline-only pages stay below 500 lines."""
        offset, chunks, finished = 0, [], False
        page_limit = 256
        for _ in range(len(expected) + 1):
            page = self.call("ssmcp.fs.read_text", {
                "path": path, "offset": offset, "max_bytes": page_limit,
            })
            if page.get("spilled") or not isinstance(page.get("text"), str):
                raise AssertionError("bounded UTF-8 page did not return inline text: "
                                     + json.dumps(page, ensure_ascii=False))
            raw = page["text"].encode("utf-8")
            end = offset + len(raw)
            if (page["path"] != path or page["offset"] != offset
                    or page["bytes"] != len(raw) or len(raw) > page_limit
                    or page["next_offset"] != end or page["size"] != len(expected)
                    or not isinstance(page["eof"], bool)
                    or not isinstance(page["truncated"], bool)
                    or page["truncated"] == page["eof"]
                    or end > len(expected)):
                raise AssertionError("native UTF-8 page changed its size, cursor or EOF contract")
            digest = hashlib.sha256(raw).hexdigest() if offset == 0 and page["eof"] else None
            if page["sha256"] != digest:
                raise AssertionError("native text page misreported its whole-file digest")
            chunks.append(raw)
            if page["eof"]:
                if end != len(expected):
                    raise AssertionError("native UTF-8 read reached EOF before the original byte length")
                finished = True
                break
            if not raw or end >= len(expected):
                raise AssertionError("native UTF-8 pagination stalled or missed EOF")
            offset = end
        actual = b"".join(chunks)
        self.check(finished and actual == expected and len(actual) == len(expected)
                   and hashlib.sha256(actual).digest() == hashlib.sha256(expected).digest(),
                   "native UTF-8 pages preserve every original byte, length and SHA-256 through EOF")
        return actual.decode("utf-8")

    def paste_scenario(self, front, member):
        prefix, suffix = "PASTE_REQUEST 分析 ", " 保留用户后缀界"
        front.send(prefix + suffix)
        self.wait_state(front, lambda state: state["draft"]["text"] == prefix + suffix,
                        "the draft surrounding the paste was not entered")
        front.send(HOME + RIGHT * len(prefix))
        pasted = ("PASTE_CONTENT 界 café\n" * 600).encode("utf-8")
        self.check(len(pasted) > 8192, "the clipboard fixture exceeds the inline paste threshold")
        self.send_stream(front, PASTE_START + pasted + PASTE_END)
        state = self.wait_state(
            front, lambda state: '@".ssmcp/inputs/' in state["draft"]["text"],
            "the long paste did not become a workspace file reference",
        )
        draft = state["draft"]["text"]
        match = re.search(r'@"(\.ssmcp/inputs/paste-[0-9a-f]+\.txt)"', draft)
        self.check(match is not None and draft.startswith(prefix) and draft.endswith(suffix),
                   "the long paste inserts an @path while preserving both surrounding draft parts")
        saved = self.workspace / match.group(1)
        self.check(saved.resolve().is_relative_to(self.workspace.resolve())
                   and saved.read_bytes() == pasted and saved.stat().st_mode & 0o077 == 0,
                   "the referenced paste file is private and contains every original UTF-8 byte")
        added = "继续说明 "
        front.send(added)
        # Native cursor offsets are UTF-8 bytes, so construct the edited text at
        # the observed byte boundary instead of confusing bytes with characters.
        raw = draft.encode("utf-8")
        cursor = state["draft"]["cursor"]
        expected = (raw[:cursor] + added.encode("utf-8") + raw[cursor:]).decode("utf-8")
        state = self.wait_state(front, lambda state: state["draft"]["text"] == expected,
                                "typing after the paste did not preserve the user suffix")
        self.save_shot("long-paste", front)
        record = self.queue_current(front, state["draft"]["text"])
        references = self.delivered_references(front, member, record, state["draft"]["text"])
        self.check(len(references) == 1 and references[0]["path"] == match.group(1),
                   "the complete pasted draft delivers exactly its saved file reference")
        read = self.call(references[0]["read"]["tool"], references[0]["read"]["arguments"])
        self.metrics["long_paste_first_read"] = read
        self.check(read.get("spilled") is True and read.get("format") == "mcp_tool_result_json"
                   and isinstance(read.get("artifact"), dict),
                   "the 600-line native result exposes the existing output artifact boundary")
        complete = self.read_text_pages(references[0]["path"], pasted)
        self.check(complete.encode("utf-8") == pasted,
                   "the receiving agent can read the whole saved paste through its native action")

    def attachment_handoff_scenario(self, front, member):
        before = {item["id"] for item in self.queue()["queue"]}
        target = self.ui("/display/conversation")["target"]
        self.check(target == member["agent_id"], "attachment fixture starts with the exact selected recipient")
        front.send('/attach "资料/红蓝.png"' + "\r")
        state = self.wait_state(front, lambda state: re.fullmatch(
            r'\n@\"\.ssmcp/inputs/image-[0-9a-f]{32}\.png\"\n', state["draft"]["text"]) is not None,
            "/attach did not prepare an editable private PNG reference")
        image_draft = state["draft"]["text"]
        image_path = re.fullmatch(r'\n@\"([^\"]+)\"\n', image_draft).group(1)
        saved_image = self.workspace / image_path
        self.check(saved_image.read_bytes() == png_fixture()
                   and saved_image.stat().st_mode & 0o777 == 0o600,
                   "/attach preserves a small PNG in a private input file")
        self.check(state["target"] == target
                   and {item["id"] for item in self.queue()["queue"]} == before,
                   "/attach keeps the selected recipient and never queues its reference automatically")
        image = self.call("ssmcp.fs.read_image", {"path": image_path})
        self.check(image["size"] == len(png_fixture())
                   and image["sha256"] == hashlib.sha256(png_fixture()).hexdigest(),
                   "the imported image remains readable through the existing rooted native image tool")
        self.save_shot("attached-image", front)
        attached = self.ui("/display/conversation")
        self.check(attached["draft"] == state["draft"]
                   and attached["draft"]["cursor"] == len(image_draft.encode("utf-8")),
                   "the private image reference is inserted between exact newlines with the cursor at its end")
        front.send(b"\x7f" * len(image_draft))
        self.wait_state(front, lambda state: not state["draft"]["text"], "attachment test draft did not clear")

        reply = "## HANDOFF_LATEST_REPLY\n\n刚核对最新上下文，界。"
        request = {"text": reply, "kind": "answer", "idempotency_key": "handoff-reply-once"}
        receipt = self.observe("ssmcp.ui.message", request)
        duplicate = self.observe("ssmcp.ui.message", request)
        self.check(receipt["accepted"] and receipt["history_saved"] and not receipt["duplicate"]
                   and receipt["kind"] == "answer" and duplicate["duplicate"]
                   and duplicate["message_id"] == receipt["message_id"],
                   "an idempotent native reply preserves its ID and is saved only once")
        pending_text = "HANDOFF_PENDING 界：尚未领取的用户要求"
        pending = self.submit_source(front, pending_text)
        self.check(pending["target"] == target, "the handoff pending fixture targets the selected real member")
        question = self.observe("ssmcp.ui.ask", {
            "question": "HANDOFF_QUESTION 界：下一步先检查哪里？",
            "options": ["输入", "历史"], "idempotency_key": "handoff-question-once",
        })
        sessions = self.observe("ssmcp.ui.history", {"action": "list"})["sessions"]
        current = next(session for session in sessions if session["current"])
        history_path = self.state_home / "ssmcp" / "conversations" / (current["id"] + ".jsonl")
        history_before = history_path.read_bytes()
        entries = [json.loads(line) for line in history_before.splitlines()]
        self.check(sum(entry.get("id") == receipt["message_id"] for entry in entries) == 1,
                   "the ui.message receipt identifies exactly one record in the shared JSONL history")

        marker = self.workspace / "handoff-must-not-run.txt"
        note = "HANDOFF_NOTE 界 $(touch handoff-must-not-run.txt)"
        before = {item["id"] for item in self.queue()["queue"]}
        front.send("/handoff " + note + "\r")
        state = self.wait_state(front, lambda state: re.fullmatch(
            r'\n@\"\.ssmcp/inputs/handoff-[0-9a-f]{32}\.md\"\n', state["draft"]["text"]) is not None,
            "/handoff did not produce a reviewable Markdown reference")
        draft = state["draft"]["text"]
        path = re.fullmatch(r'\n@\"([^\"]+)\"\n', draft).group(1)
        saved = self.workspace / path
        exported = saved.read_text(encoding="utf-8")
        self.check(saved.stat().st_mode & 0o777 == 0o600 and len(saved.read_bytes()) <= 2 * 1024 * 1024,
                   "handoff writes one private bounded Markdown input file")
        for expected in [current["id"], receipt["message_id"], reply, pending_text,
                         question["id"], question["question"]["question"], note]:
            self.check(expected in exported, "handoff contains the actual current context: " + expected[:48])
        native = self.read_text_pages(path, exported.encode("utf-8"))
        self.check(native == exported,
                   "the handoff reference is fully readable through the rooted native text tool")
        after = self.queue()
        self.check(state["target"] == target and {item["id"] for item in after["queue"]} == before
                   and self.record(after, message_id=pending["id"])["status"] == "queued",
                   "handoff preserves the recipient and pending delivery without submitting another request")
        self.check(self.observe("ssmcp.ui.questions", {"id": question["id"]})["question"]["state"] == "pending"
                   and history_path.read_bytes() == history_before and not marker.exists(),
                   "handoff does not answer questions, duplicate history, or execute shell text from its note")
        suffix = " 补充：继续核对。"
        front.send(suffix)
        self.wait_state(front, lambda state: state["draft"]["text"] == draft + suffix,
                        "the generated handoff reference was not editable")
        self.save_shot("handoff-editable", front)
        self.check(self.ui("/display/conversation")["draft"]["cursor"] == len((draft + suffix).encode("utf-8")),
                   "editing the multiline handoff reference leaves its UTF-8 cursor at the end")
        front.send(b"\x7f" * len(draft + suffix))
        self.wait_state(front, lambda state: not state["draft"]["text"], "handoff test draft did not clear")
        self.expect_delivery(self.work(member=member), pending, request_text=pending_text,
                             agent_id=member["agent_id"])
        self.check(not marker.exists(), "explicitly receiving the pending text does not execute handoff note contents")

    def copy_and_help(self, front):
        draft = "COPY_HELP_DRAFT 界后缀"
        front.send(draft)
        original = self.wait_state(front, lambda state: state["draft"]["text"] == draft,
                                   "the copy/help preservation draft was not entered")["draft"]
        offset = len(front.transcript)
        front.send(F7)
        front.until(lambda shot: "Copy mode" in text(shot), "F7 copy mode")
        modes = mouse_modes(bytes(front.transcript[offset:]))
        self.check(modes.get(1000) is False and modes.get(1006) is False,
                   "F7 disables actual terminal mouse tracking for native text selection")
        self.save_shot("copy-mode", front)
        front.send("COPY_MODE_MUST_NOT_EDIT")
        front.pump(0.15)
        self.check(self.ui("/display/conversation")["draft"] == original,
                   "copy-mode input leaves the UTF-8 draft and cursor unchanged")
        offset = len(front.transcript)
        front.send(F7)
        front.until(lambda shot: "Copy mode" not in text(shot), "F7 leaving copy mode")
        modes = mouse_modes(bytes(front.transcript[offset:]))
        self.check(modes.get(1000) is True and modes.get(1006) is True,
                   "F7 restores actual mouse tracking after selection")

        front.send(F1)
        front.until(lambda shot: "Write and send" in text(shot), "the beginning of full help")
        front.send(END)
        last = "Help: Up/Down, PgUp/PgDn, Home/End scroll; Esc returns."
        front.until(lambda shot: last in text(shot) and "Native harnesses" in text(shot),
                    "the final help section after scrolling to End")
        self.save_shot("help-end", front)
        self.check(True, "the full help document can scroll to its final line")
        front.send(HOME)
        front.until(lambda shot: "Write and send" in text(shot), "Home returning help to its beginning")
        front.send(ESC)
        front.pump(0.15)
        self.check(self.ui("/display/conversation")["draft"] == original,
                   "help scrolling and Escape preserve the original draft and cursor")
        front.send(b"\x01\x0b")
        self.wait_state(front, lambda state: not state["draft"]["text"],
                        "the test-owned preservation draft did not clear")

    def history_scenario(self, front, first_source):
        counter = self.workspace / "tool-once.txt"
        job = self.run_job([
            "python3", "-c",
            "from pathlib import Path; p=Path('tool-once.txt'); "
            "p.write_text((p.read_text() if p.exists() else '') + 'once\\n')",
        ])
        self.finish_job(job, front)
        self.check(counter.read_text() == "once\n", "the owned native tool fixture executes exactly once")
        reply = "# HISTORY_REPLY_FIXTURE\n\n已读取文件、技能和图片。"
        self.observe("ssmcp.ui.message", {"text": reply})
        sessions = self.observe("ssmcp.ui.history", {"action": "list"})["sessions"]
        self.check(len(sessions) == 1, "user requirements and the agent reply share one automatic history")
        session_id = sessions[0]["id"]
        path = self.state_home / "ssmcp" / "conversations" / f"{session_id}.jsonl"
        raw = path.read_bytes()
        entries = [json.loads(line) for line in raw.splitlines()]
        self.check(entries[0]["type"] == "session"
                   and entries[0]["schema"] == "ssmcp.conversation.v1"
                   and entries[0]["schema_version"] == 1,
                   "automatic history is a versioned JSONL stream with session metadata")
        messages = [entry for entry in entries[1:] if entry["type"] == "message"]
        self.check(all(item["role"] in {"system", "user", "assistant", "tool"}
                       and isinstance(item["content"], str) for item in messages)
                   and any(item["role"] == "user" and item["content"] == first_source for item in messages)
                   and any(item["role"] == "assistant" and item["content"] == reply for item in messages),
                   "JSONL records the original user text and agent reply using standard message roles")
        (self.output / "input-history-before.jsonl").write_bytes(raw)
        self.save_shot("before-restart", front)
        self.metrics["first_pty"] = {
            "bytes": len(front.transcript), "sha256": hashlib.sha256(front.transcript).hexdigest(),
        }
        front.disconnect()
        front.close()

        resumed = self.start_front("input-history-restarted")
        saved = self.observe("ssmcp.ui.history", {"action": "list"})["sessions"]
        selected = next(index for index, item in enumerate(saved) if item["id"] == session_id)
        resumed.send("/resume\r")
        resumed.until(lambda shot: "ssmcp  /  history" in text(shot) and session_id in text(shot),
                      "the persisted conversation in the /resume picker")
        resumed.send(HOME + DOWN * selected + ENTER)
        resumed.until(lambda shot: "HISTORY_REPLY_FIXTURE" in text(shot),
                      "the saved agent reply restored in the new process")
        self.save_shot("resumed", resumed)
        self.check(path.read_bytes() == raw, "viewing a saved conversation does not duplicate its JSONL records")
        self.check(not self.queue()["queue"] and not self.messages(self.work()),
                   "resuming in a new process neither restores nor redelivers old queued requirements")
        self.check(counter.read_text() == "once\n", "resuming history does not replay the old native tool")

        continuation = "HISTORY_CONTINUATION 界"
        record = self.submit_source(resumed, continuation)
        self.expect_delivery(self.work(), record, request_text=continuation, agent_id=None)
        history = self.observe("ssmcp.ui.history", {"action": "read", "id": session_id, "limit": 64})
        self.check(history["messages"][-1]["role"] == "user"
                   and history["messages"][-1]["content"] == continuation
                   and len(history["messages"]) == len(messages) + 1,
                   "a new requirement continues the selected saved conversation exactly once")
        (self.output / "input-history-after.jsonl").write_bytes(path.read_bytes())
        self.save_shot("continued", resumed)
        self.metrics["restarted_pty"] = {
            "bytes": len(resumed.transcript), "sha256": hashlib.sha256(resumed.transcript).hexdigest(),
        }
        self.metrics["history_messages"] = len(history["messages"])
        resumed.close()

    def shell_scenario(self, front):
        before = len(self.queue()["queue"])
        front.send("!printf '%s_%s' SHELL_PREFIX OK; exit 7")
        front.send(ENTER)
        front.until(lambda shot: "command · exit 7" in text(shot),
                    "the explicit shell command completes and shows its exit status")
        state = self.ui("/display/conversation")
        self.check(not state["draft"]["text"] and len(self.queue()["queue"]) == before,
                   "the ! command clears its draft without entering the agent request queue")
        self.check("SHELL_PREFIX_OK" in bytes(front.transcript).decode("utf-8", errors="replace"),
                   "the real shell command output reaches the terminal")
        self.save_shot("shell-command", front)
        front.send("/tasks")
        front.send(ENTER)
        front.until(lambda shot: "Local tasks" in text(shot) and "exit 7" in text(shot),
                    "the local task list includes the completed shell job")
        self.check(len(self.queue()["queue"]) == before,
                   "/tasks remains a local command instead of an agent requirement")
        self.save_shot("tasks", front)

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(
            ["git", "-c", "init.defaultBranch=main", "init", "-q"], cwd=self.workspace,
            env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5,
        )
        front = self.start_front("input-history-first")
        member = self.public_call("collab", {
            "action": "join", "client_id": self.client_id,
            "name": "Input fixture member", "role": "local input/history test",
        })
        self.sensitive.append(member["token"])
        self.agents(front)
        front.until(lambda shot: "Input fixture member" in text(shot),
                    "the registered fixture member arriving in the live roster")
        roster = self.queue()["agents"]
        selected = 1 + next(index for index, item in enumerate(roster)
                            if item["agent_id"] == member["agent_id"])
        front.send(HOME + DOWN * selected + RIGHT)
        front.until(lambda shot: "To · Input fixture member" in text(shot)
                    and "ssmcp  /  activity" in text(shot), "the exact registered fixture recipient")
        first_source = self.reference_scenarios(front, member)
        self.paste_scenario(front, member)
        self.attachment_handoff_scenario(front, member)
        self.copy_and_help(front)
        self.shell_scenario(front)
        self.history_scenario(front, first_source)


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("input/history E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--decoder", type=Path, help="defaults to BIN_DIR/examples/vt_snapshot")
    parser.add_argument("--out", type=Path, required=True, help="write JSON report and actual VT snapshots")
    parser.add_argument("--report", type=Path, help="also write the report at this location")
    args = parser.parse_args()
    decoder = args.decoder or args.bin_dir / "examples/vt_snapshot"
    binaries = {"ssmcp": args.bin_dir / "ssmcp", "ssmcpd": args.bin_dir / "ssmcpd", "vt_snapshot": decoder}
    for executable in binaries.values():
        if not executable.is_file():
            parser.error(f"missing {executable}; build the binaries and vt_snapshot first")
    args.out.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    report = {
        "status": "failed", "platform": platform.system(),
        "transport": "isolated OpenSSH public-key + OAuth + owned PTY + independent vt100 decoder",
        "scope": "enumerated @/$ completion, file/skill/image delivery, large paste, private image attach, editable handoff, idempotent replies, copy/help, local shell/tasks, automatic JSONL and resume; no models",
        "binaries": {name: sha256(path) for name, path in binaries.items()},
        "sources": {name: sha256(Path(__file__).with_name(name)) for name in (
            "e2e_input_history.py", "e2e.py", "e2e_dashboard.py", "e2e_composer.py",
        )},
    }
    with tempfile.TemporaryDirectory(prefix="ssmcp-input-history-e2e-") as directory:
        harness = InputHistory(Path(directory), args.bin_dir, decoder, args.out)
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
            if harness.last_frontend and not harness.last_frontend.closed:
                try:
                    harness.save_shot("failure", harness.last_frontend, cells=True)
                except (OSError, AssertionError) as snapshot_error:
                    report["snapshot_error"] = type(snapshot_error).__name__
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            encoded = json.dumps(report, ensure_ascii=False, indent=2) + "\n"
            (args.out / "report.json").write_text(encoded, encoding="utf-8")
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(encoded, encoding="utf-8")
    if report["status"] != "passed":
        raise SystemExit(1)
    print(f"\nPassed {len(report['checks'])} input/history checks in {report['duration_seconds']}s", flush=True)


if __name__ == "__main__":
    main()
