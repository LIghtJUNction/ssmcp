#!/usr/bin/env python3
"""Exercise image/file pastes, session messages and /copy in an owned POSIX PTY.

Build ssmcp, ssmcpd and examples/vt_snapshot first, then run:
  python3 scripts/e2e_attachments.py --bin-dir target/release --report /tmp/attachments.json

Reuses the isolated SSH/OAuth composer harness and independent VT decoder.
The only clipboard writers on the client's PATH are test-owned Python fixtures.
Their private Unix-socket owner never connects to the desktop clipboard. No
installed coding harness, model, live session or real clipboard is accessed.
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
import socket
import struct
import subprocess
import tempfile
import time
import zlib

from e2e_composer import Composer, ENTER, LEFT, input_text, visible_record
from e2e_dashboard import sha256, text


CLEAR_LINE = b"\x01\x0b"
PROJECT = "attachment-project"
COPY_TEXT = "ATTACHMENT_AGENT_ANSWER\nExact copied body: 中文, **Markdown**, and a final newline.\n"

# This emulates wl-copy/xclip's short launcher and long-lived selection owner.
# The child intentionally keeps the launcher's process group: a managed
# capture helper that kills that group on return would break the socket test.
# It never opens an X11/Wayland connection or calls a real clipboard utility.
COPY_OWNER = r'''#!/usr/bin/env python3
import hashlib, json, os, select, socket, sys, time
from pathlib import Path

root = Path(os.environ["SSMCP_E2E_CLIPBOARD"])
data = sys.stdin.buffer.read(1024 * 1024 + 1)
if not data or len(data) > 1024 * 1024:
    raise SystemExit(2)
ready_read, ready_write = os.pipe()
child = os.fork()
if child:
    os.close(ready_write)
    ready, _, _ = select.select([ready_read], [], [], 2)
    accepted = bool(ready) and os.read(ready_read, 1) == b"1"
    os.close(ready_read)
    raise SystemExit(0 if accepted else 3)

os.close(ready_read)
endpoint = root / "owner.sock"
server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
try:
    with open(os.devnull, "rb") as empty:
        os.dup2(empty.fileno(), 0)
    server.bind(str(endpoint))
    server.listen(4)
    server.settimeout(0.2)
    metadata = {"pid": os.getpid(), "pgid": os.getpgrp(),
                "bytes": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    temporary = root / "owner.tmp"
    temporary.write_text(json.dumps(metadata))
    temporary.replace(root / "owner.json")
    os.write(ready_write, b"1")
    os.close(ready_write)
    deadline = time.monotonic() + 120
    while time.monotonic() < deadline and not (root / "stop").exists():
        try:
            connection, _ = server.accept()
        except socket.timeout:
            continue
        with connection:
            connection.settimeout(1)
            request = connection.recv(16)
            if request == b"stop\n":
                connection.sendall(b"stopped")
                break
            if request != b"read\n":
                continue
            connection.sendall(data)
finally:
    server.close()
    endpoint.unlink(missing_ok=True)
    os._exit(0)
'''

# Read-side fixture for the actual Rust wl-paste adapter; no desktop connection.
PASTE_READER = r'''#!/usr/bin/env python3
import base64, json, os, sys, time
from pathlib import Path
root = Path(os.environ["SSMCP_E2E_CLIPBOARD"])
selection = json.loads((root / "selection.json").read_text())
if "--list-types" in sys.argv:
    print("\n".join(selection["payloads"]))
else:
    flag = "--type" if "--type" in sys.argv else "-t"
    target = sys.argv[sys.argv.index(flag) + 1]
    with (root / "reads.log").open("a") as log:
        log.write(target + "\n")
    time.sleep(selection.get("delay", 0))
    sys.stdout.buffer.write(base64.b64decode(selection["payloads"][target], validate=True))
'''


def png_fixture():
    """A precise 2x2 RGBA PNG made with the standard library, including alpha."""
    def chunk(kind, payload):
        return (struct.pack(">I", len(payload)) + kind + payload
                + struct.pack(">I", zlib.crc32(kind + payload) & 0xffffffff))

    pixels = (b"\x00" + bytes((255, 0, 0, 255, 0, 255, 0, 128))
              + b"\x00" + bytes((0, 0, 255, 0, 255, 255, 255, 255)))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", 2, 2, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(pixels, 9)) + chunk(b"IEND", b""))


class Attachments(Composer):
    def __init__(self, directory, binary_dir, decoder):
        super().__init__(directory, binary_dir, decoder)
        self.clipboard = directory / "clipboard"
        self.clipboard.mkdir(mode=0o700)
        self.png = png_fixture()
        self.image_id = hashlib.sha256(self.png).hexdigest()
        self.select_clipboard("text/plain;charset=utf-8", b"")

    def config(self, name, **_options):
        path = self.directory / f"{name}.toml"
        path.write_text(
            '[ui]\ndashboard=true\ncolor=false\nlog_format="pretty"\n'
            'timestamps=false\nlayout="chat"\nsidebar=true\n'
        )
        return path

    def client_environment(self, label, **options):
        environment = super().client_environment(label, **options)
        # The inherited harness already hides real model executables. Also hide
        # real display endpoints and supply ONLY our private clipboard writers.
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET",
                    "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS"):
            environment.pop(key, None)
        environment["WAYLAND_DISPLAY"] = "ssmcp-e2e-private-clipboard"
        environment["XDG_RUNTIME_DIR"] = str(self.clipboard)
        environment["SSMCP_E2E_CLIPBOARD"] = str(self.clipboard)
        for name in ("wl-copy", "xclip", "pbcopy"):
            program = Path(environment["PATH"]) / name
            program.write_text(COPY_OWNER)
            program.chmod(0o700)
        reader = Path(environment["PATH"]) / "wl-paste"
        reader.write_text(PASTE_READER)
        reader.chmod(0o700)
        return environment

    def select_clipboard(self, mime, data, *, delay=0):
        (self.clipboard / "selection.json").write_text(json.dumps({
            "payloads": {mime: base64.b64encode(data).decode()}, "delay": delay,
        }))

    def paste_reads(self):
        path = self.clipboard / "reads.log"
        return len(path.read_text().splitlines()) if path.exists() else 0

    def send_file_draft(self, front, sources, *, image):
        draft = self.ui("/display/conversation")["draft"]["text"]
        before = {item["id"] for item in self.queue()["queue"]}
        self.check(not self.messages(self.work()),
                   "pasted files stay local until the user presses Enter")
        front.send(ENTER)
        self.wait_ui(front, lambda state: not state["draft"]["text"]
                     and not state["draft"].get("images"), "file request acknowledgement")
        deadline = time.monotonic() + 10
        record = None
        while time.monotonic() < deadline:
            added = [item for item in self.queue()["queue"] if item["id"] not in before]
            if len(added) == 1 and added[0]["status"] == "queued":
                record = added[0]
                break
            front.pump(0.05)
        self.check(record is not None, "Enter queues one complete file request")
        response = self.work()
        messages = self.messages(response)
        self.check(len(messages) == 1 and messages[0]["message_id"] == record["id"],
                   "the next eligible tool receives the submitted file request once")
        original, encoded = messages[0]["text"].rsplit("\n\n<ssmcp-references-v1>\n", 1)
        payload = json.loads(encoded.removesuffix("\n</ssmcp-references-v1>"))
        self.check(original == draft and payload["source_sha256"] == hashlib.sha256(draft.encode()).hexdigest(),
                   "file expansion preserves the user's exact draft and its identity")
        entries = payload["references"]
        self.check(len(entries) == len(sources), "each ordinary file becomes one bounded reference")
        for entry, source in zip(entries, sources):
            copied = self.workspace / entry["path"]
            self.check(copied.name == source.name and copied.read_bytes() == source.read_bytes()
                       and entry["size"] == source.stat().st_size
                       and copied.stat().st_mode & 0o777 == 0o600
                       and copied.parent.stat().st_mode & 0o777 == 0o700,
                       "imported names, exact file bytes, sizes and private permissions are retained")
            native = self.response("return await call(%s, 'ssmcp.fs.read', %s);" % (
                json.dumps(self.client_id), json.dumps({"path": entry["path"], "max_bytes": 65536}),
            ))["structuredContent"]["result"]
            self.check(not native.get("isError")
                       and base64.b64decode(native["structuredContent"]["data_base64"], validate=True) == source.read_bytes(),
                       "the delivered reference can be read through the client's normal workspace API")
        blocks = [block for block in response["content"] if block.get("type") == "image"]
        self.check((len(blocks) == 1 and blocks[0]["mimeType"] == "image/png"
                    and base64.b64decode(blocks[0]["data"], validate=True) == self.png) if image else not blocks,
                   "mixed clipboard files retain the exact PNG as an MCP image block")
        self.no_payload(messages, "file request metadata")
        self.check(not self.messages(self.work()), "the file request is not replayed")

    def file_pastes(self, front):
        self.local(front, "/queue", view="queue")
        external = self.directory / "copied files"
        external.mkdir()
        note = external / 'notes "中文" draft.txt'
        binary = external / "sample report.pdf"
        note.write_text("Explicit copied file; original source remains untouched.\n")
        binary.write_bytes(b"%PDF-1.4\nfixture\x00bytes\n")
        originals = {path: path.read_bytes() for path in (note, binary, self.fixture)}

        self.select_clipboard("image/png", self.png)
        front.send(b"\x16")
        state = self.wait_ui(front, lambda current: not current.get("loading_image")
                             and len(current["draft"].get("images", [])) == 1,
                             "Ctrl+V reading a PNG from the private clipboard")
        self.metadata(state["draft"]["images"], "raw clipboard PNG")
        self.local(front, "/detach all")

        self.select_clipboard("text/uri-list", (
            "# copied files\r\n" + "\r\n".join(path.as_uri() for path in (note, binary, self.fixture)) + "\r\n"
        ).encode(), delay=0.2)
        prefix = "FILE_PASTE_KEEP 界"
        front.send(prefix.encode() + LEFT)
        self.wait_ui(front, lambda state: state["draft"]["text"] == prefix,
                     "an existing Unicode draft before importing files")
        front.send(b"\x16AFTER")
        state = self.wait_ui(front, lambda current: not current.get("loading_image")
                             and '@".ssmcp/inputs/files-' in current["draft"]["text"]
                             and len(current["draft"].get("images", [])) == 1,
                             "mixed URI file and PNG import into the live composer")
        draft = state["draft"]
        self.check(draft["text"].startswith("FILE_PASTE_KEEP ") and draft["text"].endswith("AFTER界")
                   and draft["cursor"] == len(draft["text"].encode()) - len("界".encode()),
                   "async file paste retains the anchor, later typing and UTF-8 cursor")
        references = re.findall(r'@"((?:\\.|[^"\\])*)"', draft["text"])
        self.check(len(references) == 2 and all(
            (self.workspace / re.sub(r'\\([\\"])', r'\1', path)).is_file() for path in references
        ), "quoted filenames with spaces and literal quotes remain valid workspace references")
        self.send_file_draft(front, (note, binary), image=True)

        self.select_clipboard("x-special/gnome-copied-files", ("cut\n" + note.as_uri() + "\n").encode())
        front.send(b"/paste\r")
        self.wait_ui(front, lambda state: not state.get("loading_image")
                     and '@".ssmcp/inputs/files-' in state["draft"]["text"],
                     "/paste importing a GNOME cut selection without moving its source")
        self.send_file_draft(front, (note,), image=False)
        self.check(note.read_bytes() == originals[note], "GNOME cut selections are copied without deleting sources")

        front.send(b"\x1b[200~" + json.dumps(str(binary), ensure_ascii=False).encode() + b"\x1b[201~")
        self.wait_ui(front, lambda state: not state.get("loading_image")
                     and '@".ssmcp/inputs/files-' in state["draft"]["text"],
                     "a bracketed full file path reusing the same safe import")
        self.send_file_draft(front, (binary,), image=False)

        self.select_clipboard("image/png", self.png)
        front.send(b"\x16")
        self.wait_ui(front, lambda state: not state.get("loading_image")
                     and len(state["draft"].get("images", [])) == 1, "an image draft before file errors")
        front.send(b"FILE_PASTE_FAILURE_KEEP" + LEFT)
        before = self.wait_ui(front, lambda state: state["draft"]["text"] == "FILE_PASTE_FAILURE_KEEP",
                              "the draft to preserve through file import errors")["draft"]
        existing = sorted(str(path) for path in (self.workspace / ".ssmcp/inputs").glob("files-*"))
        link = external / "symlink.txt"
        link.symlink_to(note)
        for uri in (link.as_uri(), "https://example.invalid/do-not-download"):
            reads = self.paste_reads()
            self.select_clipboard("text/uri-list", (uri + "\n").encode(), delay=0.1)
            front.send(b"\x16")
            state = self.wait_ui(front, lambda state: self.paste_reads() > reads
                                 and not state.get("loading_image"), "safe file-paste rejection")
            self.check(state["draft"] == before, "rejected file pastes preserve existing text, cursor and images")
        self.check(existing == sorted(str(path) for path in (self.workspace / ".ssmcp/inputs").glob("files-*")),
                   "rejected file selections leave no new import directories")
        self.check(all(path.read_bytes() == original for path, original in originals.items()),
                   "all copied and cut source files remain unchanged")
        front.send(CLEAR_LINE + b"/detach all\r")
        self.wait_ui(front, lambda state: not state["draft"]["text"] and not state["draft"].get("images"),
                     "clearing only the test-owned failure draft")
        self.local(front, "/logs", view="activity")
        self.metrics["file_pastes"] = {"requests": 3, "clipboard_reads": self.paste_reads(),
                                       "sources_preserved": True, "network_downloads": False}

    def wait_ui(self, front, predicate, description, *, timeout=10):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            front.pump(0.05)
            state = self.ui("/display/conversation")
            if predicate(state):
                return state
            if front.process.poll() is not None:
                raise AssertionError(f"client exited while waiting for {description}")
        raise AssertionError(f"timed out waiting for {description}\n{front.failure_detail()}")

    def local(self, front, command, *, view=None):
        self.focus_input(front)
        if self.ui("/display/conversation")["draft"]["text"]:
            raise AssertionError("local command would replace a nonempty test draft")
        front.send(command + "\r")
        return self.wait_ui(
            front, lambda state: not state["draft"]["text"]
            and (view is None or state["view"] == view),
            f"local command {command}",
        )

    def metadata(self, images, description):
        self.check(len(images) == 1, f"{description}: one attached image")
        image = images[0]
        self.check(
            image["id"] == self.image_id and image["mime_type"] == "image/png"
            and image["bytes"] == len(self.png)
            and image["encoded_bytes"] == len(base64.b64encode(self.png))
            and "2×2" in image["name"] and "data" not in image,
            f"{description}: exact PNG identity, dimensions and metadata without image data",
        )

    def no_payload(self, value, description):
        encoded = base64.b64encode(self.png).decode()
        self.check(encoded not in json.dumps(value), f"{description}: base64 is absent")

    def attach(self, front):
        self.focus_input(front)
        state = self.ui("/display/conversation")
        if state["draft"]["text"] or state["draft"].get("images"):
            raise AssertionError("attach helper expects an empty draft")
        front.send(f'/attach "{self.fixture}"\r')
        state = self.wait_ui(
            front, lambda current: not current.get("loading_image")
            and len(current["draft"].get("images", [])) == 1,
            "image decoding and attachment to the actual composer",
        )
        front.until(lambda shot: "[Image 1]" in input_text(shot)
                    and "2×2" in input_text(shot), "attachment chip and actual dimensions")
        self.metadata(state["draft"]["images"], "composer attachment")
        self.no_payload(state, "composer snapshot")
        self.metadata(self.queue()["draft"]["images"], "persisted local draft")

    def take_back(self, front, record):
        self.focus_input(front)
        # Write the real modified-arrow sequence in two pieces, within the
        # terminal decoder's Escape timeout; ordinary Up history is not used.
        front.send(b"\x1b[1;")
        time.sleep(0.005)
        front.send(b"2A")
        state = self.wait_ui(
            front, lambda current: current["editing"] == record["id"],
            "Shift+Up reclaiming the latest queued image request",
        )
        self.metadata(state["draft"]["images"], "reclaimed request")
        held_record = self.record(self.queue(), message_id=record["id"])
        self.check(held_record["status"] == "editing"
                   and held_record["revision"] > record["revision"],
                   "Shift+Up enters the editor after the server confirms CAS takeback")
        held = self.work()
        self.check(not self.messages(held)
                   and not any(block.get("type") == "image" for block in held["content"]),
                   "an image request held for editing is not delivered")

    def delivery(self, response, record, expected_text, *, image=True):
        self.expect_delivery(response, record, expected_text)
        blocks = [block for block in response["content"] if block.get("type") == "image"]
        if image:
            self.check(len(blocks) == 1 and blocks[0]["mimeType"] == "image/png"
                       and base64.b64decode(blocks[0]["data"], validate=True) == self.png,
                       "the next eligible tool response carries the exact PNG as an MCP image block")
            self.metadata(self.messages(response)[0]["images"], "delivered image metadata")
        else:
            self.check(not blocks and not self.messages(response)[0].get("images"),
                       "removing the attachment removes the actual MCP image delivery")
        self.no_payload(self.messages(response), "structured user message metadata")

    def clipboard_read(self, request=b"read\n"):
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(2)
            connection.connect(str(self.clipboard / "owner.sock"))
            connection.sendall(request)
            data = bytearray()
            while True:
                block = connection.recv(65536)
                if not block:
                    return bytes(data)
                data.extend(block)
                if len(data) > 1024 * 1024:
                    raise AssertionError("private clipboard owner exceeded its fixed limit")

    def stop_clipboard(self):
        (self.clipboard / "stop").touch()
        endpoint = self.clipboard / "owner.sock"
        if endpoint.exists():
            try:
                self.clipboard_read(b"stop\n")
            except (OSError, ConnectionError):
                pass
        deadline = time.monotonic() + 2
        while endpoint.exists() and time.monotonic() < deadline:
            time.sleep(0.03)
        if endpoint.exists():
            raise AssertionError("the owned fake clipboard process did not release its socket")

    @staticmethod
    def footer_has_project(shot):
        footer = shot["rows"][-1]
        return footer.rfind(PROJECT) >= len(footer) // 2

    def run(self):
        self.prepare()
        self.workspace = self.directory / PROJECT
        self.workspace.mkdir()
        subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "-q"],
                       cwd=self.workspace, env={**os.environ, **self.safe_git_env},
                       stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
                       check=True, timeout=5)
        self.fixture = self.workspace / "capture one.png"
        self.fixture.write_bytes(self.png)
        front = self.frontend("attachments", self.config("attachments"))
        self.oauth_login(front.credentials)
        front.until(lambda shot: shot["alternate_screen"] and "To · next caller" in text(shot),
                    "the real connected chat composer")
        self.check(self.queue()["images_supported"], "SSH negotiated image-capable wire version 8")

        front.until(self.footer_has_project, "project name at the lower right with the sidebar expanded")
        self.check(self.ui("/config/sidebar") is True, "the fixture begins with a visible sidebar")
        self.local(front, "/sidebar")
        deadline = time.monotonic() + 5
        while self.ui("/config/sidebar") is not False and time.monotonic() < deadline:
            front.pump(0.05)
        self.check(self.ui("/config/sidebar") is False, "the actual /sidebar command folds the sidebar")
        front.until(self.footer_has_project, "project name retained at the lower right after folding")
        self.check(True, "the footer project name is independent of sidebar visibility")

        self.file_pastes(front)

        self.attach(front)
        self.local(front, "/queue", view="queue")
        first = self.submit(front, "ATTACHMENT_EDIT_ORIGINAL")
        self.metadata(first["images"], "queued image")
        self.no_payload(self.queue(), "queue snapshot")
        # A second, unfinished text-and-image draft must survive taking back
        # the first request, including the cursor inside a UTF-8 logical line.
        self.attach(front)
        pending_text = "SHIFT_UP_PARKED_DRAFT 界"
        front.send(pending_text.encode() + LEFT)
        pending_state = self.wait_ui(
            front, lambda current: current["draft"]["text"] == pending_text
            and current["draft"]["cursor"] == len(pending_text.encode()) - len("界".encode()),
            "the independent image draft before Shift+Up",
        )
        pending_draft = pending_state["draft"]
        self.take_back(front, first)
        parked_draft = self.queue()["draft_context"]["parked"]
        self.check(parked_draft is not None
                   and all(parked_draft[key] == pending_draft[key]
                           for key in ("text", "cursor", "images")),
                   "confirmed Shift+Up parks the current text, image and UTF-8 cursor")
        self.metadata(parked_draft["images"], "Shift+Up parked draft")
        revised = "ATTACHMENT_EDIT_REVISED"
        # The editor already owns focus. Sending Escape here would deliberately
        # restore an unchanged parked draft, so submit the actual edit directly.
        front.send(CLEAR_LINE + revised.encode() + ENTER)
        front.until(lambda shot: visible_record(shot, revised, "queued", first["id"]),
                    "the Shift+Up edit being acknowledged under its original identity")
        self.wait_ui(front, lambda current: current["draft"] == pending_draft
                     and current["editing"] is None,
                     "the parked image draft restored after submitting the edit")
        edited = self.record(self.queue(), message_id=first["id"])
        self.check(edited["revision"] > first["revision"], "editing retains identity and advances revision")
        self.metadata(edited["images"], "edited queue item")
        self.delivery(self.work(), first, revised)
        self.wait_delivered(front, first, revised)
        front.send(CLEAR_LINE + b"/detach all\r")
        self.wait_ui(front, lambda current: not current["draft"]["text"]
                     and not current["draft"].get("images"),
                     "clearing only the restored test-owned draft for the next scenario")

        self.attach(front)
        removed = self.submit(front, "ATTACHMENT_REMOVE_ORIGINAL")
        self.take_back(front, removed)
        before_delete = self.ui("/display/conversation")
        before_target = self.queue()["draft_context"]["target"]
        original_text = before_delete["draft"]["text"]
        front.send(b"x")
        self.wait_ui(front, lambda state: state["draft"]["text"] == original_text + "x",
                     "typing beside the held image before normal Backspace")
        front.send(b"\x7f")
        self.wait_ui(front, lambda state: state["draft"]["text"] == original_text
                     and len(state["draft"].get("images", [])) == 1,
                     "ordinary Backspace deleting text before images")
        front.send(b"\x01\x7f")
        no_image = self.wait_ui(front, lambda state: not state["draft"].get("images")
                               and state["draft"]["text"] == original_text
                               and state["draft"]["cursor"] == 0,
                               "Backspace at draft start removing the last image")
        self.check(no_image["editing"] == removed["id"]
                   and self.queue()["draft_context"]["target"] == before_target,
                   "Backspace image removal preserves the held edit, full text and chosen target")
        front.until(lambda shot: "[Image 1]" not in input_text(shot),
                    "the removed image chip disappearing from the actual input")
        without_image = "ATTACHMENT_REMOVE_REVISED"
        front.send(CLEAR_LINE + without_image.encode() + ENTER)
        front.until(lambda shot: visible_record(shot, without_image, "queued", removed["id"]),
                    "submitting an edited request after Backspace removes its image")
        removed_now = self.record(self.queue(), message_id=removed["id"])
        self.check(not removed_now["images"] and removed_now["target"] == removed["target"],
                   "Backspace removal is persisted atomically with the edited request and target")
        self.delivery(self.work(), removed, without_image, image=False)
        self.wait_delivered(front, removed, without_image)

        # Park a real text-and-image draft by copying delivered history, then
        # use the local /reload command. The command restores the parked draft
        # before reexec, including its UTF-8 cursor and original image bytes.
        self.attach(front)
        parked = "ATTACHMENT_DRAFT_AFTER_RELOAD 界"
        front.send(parked.encode() + LEFT)
        self.wait_ui(front, lambda state: state["draft"]["text"] == parked,
                     "the unfinished image draft before reload")
        parked_cursor = self.queue()["draft"]["cursor"]
        self.select_request(front, first["id"])
        front.send(b"r")
        self.wait_ui(front, lambda state: state["draft"]["text"] == revised,
                     "copying delivered history while parking the image draft")
        before = self.queue()
        self.metadata(before["draft_context"]["parked"]["images"], "parked image draft")
        self.no_payload(before, "parked draft snapshot")
        old_id, old_token, old_pid = self.client_id, self.secret, front.process.pid
        front.send(CLEAR_LINE + b"/reload\r")
        credentials = self.new_credentials(front)
        self.check(front.process.pid == old_pid and credentials["client_id"] != old_id,
                   "/reload reexecs the owned client and creates a new session")
        front.credentials = credentials
        front.client_id = self.client_id = credentials["client_id"]
        front.until(lambda shot: parked in input_text(shot) and "[Image 1]" in input_text(shot),
                    "text and image attachment restored in the reexecuted TUI")
        status, _, _ = self.rpc("ping", modern=True, secret=old_token)
        self.check(status == 401, "the old login-code grant is revoked after reload")
        self.oauth_login(credentials)
        restored = self.queue()
        self.check(restored["draft"]["text"] == parked
                   and restored["draft"]["cursor"] == parked_cursor
                   and restored["draft_context"]["parked"] is None,
                   "reload restores the visible draft and UTF-8 cursor without a stranded parked copy")
        self.metadata(restored["draft"]["images"], "reload-restored attachment")
        self.no_payload(restored, "reload queue snapshot")
        front.until(self.footer_has_project, "project context after reexec")
        # Three F2 presses reach Queue while preserving the nonempty draft.
        front.send(b"\x1bOQ" * 3)
        self.wait_ui(front, lambda state: state["view"] == "queue", "queue view after reload")
        restored_record = self.submit(front, parked)
        self.delivery(self.work(), restored_record, parked)
        self.wait_delivered(front, restored_record, parked)
        self.check(not self.messages(self.work()), "a delivered image request is not replayed")

        self.local(front, "/logs", view="activity")
        progress_text = "ATTACHMENT_AGENT_PROGRESS: checking the pasted image."
        progress = self.observe("ssmcp.ui.message", {
            "text": progress_text, "kind": "progress", "author": "Attachment fixture",
            "idempotency_key": "attachment-progress",
        })
        front.until(lambda shot: "[UPDATE] Attachment fixture" in text(shot)
                    and progress_text in text(shot), "native progress message in the physical terminal")
        answer = self.observe("ssmcp.ui.message", {
            "text": COPY_TEXT, "kind": "answer", "author": "Attachment fixture",
            "idempotency_key": "attachment-answer",
        })
        front.until(lambda shot: "[ANSWER] Attachment fixture" in text(shot)
                    and "ATTACHMENT_AGENT_ANSWER" in text(shot), "native answer in the physical terminal")
        history = self.observe("ssmcp.ui.messages", {"limit": 8})
        self.check(progress["accepted"] and answer["accepted"] and answer["terminal_active"],
                   "native user-facing messages are accepted by the active client terminal")
        self.check([item["text"] for item in history["messages"]] == [progress_text, COPY_TEXT]
                   and history["messages"][-1]["id"] == answer["message_id"],
                   "message history retains the exact progress and latest answer bodies")

        self.attach(front)
        images_before_copy = self.ui("/display/conversation")["draft"]["images"]
        front.send(b"/copy\r")
        front.until(lambda shot: "Latest agent message copied" in shot["rows"][-1],
                    "the asynchronous /copy success notice")
        self.wait_ui(front, lambda state: not state.get("loading_image")
                     and not state["draft"]["text"], "clipboard completion without changing the draft")
        self.check(self.ui("/display/conversation")["draft"]["images"] == images_before_copy,
                   "/copy preserves the draft image attachments")
        self.check(self.clipboard_read() == COPY_TEXT.encode(),
                   "/copy writes the exact latest agent message to the isolated clipboard owner")
        owner = json.loads((self.clipboard / "owner.json").read_text())
        self.check(owner["pgid"] != front.process.pid,
                   "the clipboard owner is independent of the client terminal process group")

        final, restored_termios = front.disconnect()
        self.check(not final["alternate_screen"] and restored_termios == front.original_termios
                   and front.process.returncode == 0,
                   "the reexecuted client exits successfully and restores the owned terminal")
        self.check(self.clipboard_read() == COPY_TEXT.encode(),
                   "the clipboard owner still serves the copied answer after client exit")
        self.stop_clipboard()
        self.metrics["attachments"] = {
            "pty_bytes": len(front.transcript),
            "pty_sha256": hashlib.sha256(front.transcript).hexdigest(),
            "png_bytes": len(self.png), "png_sha256": self.image_id,
            "copied_bytes": owner["bytes"], "copied_sha256": owner["sha256"],
            "request_count": 6,
        }
        front.close()


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("attachment E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--decoder", type=Path, help="defaults to BIN_DIR/examples/vt_snapshot")
    parser.add_argument("--report", type=Path, help="write assertions and hashes without credentials or image bytes")
    args = parser.parse_args()
    decoder = args.decoder or args.bin_dir / "examples/vt_snapshot"
    for executable in (args.bin_dir / "ssmcp", args.bin_dir / "ssmcpd", decoder):
        if not executable.is_file():
            parser.error(f"missing {executable}; build the binaries and vt_snapshot first")
    started = time.monotonic()
    report = {
        "status": "failed", "platform": platform.system(),
        "transport": "isolated OpenSSH + OAuth + owned PTY + independent VT decoder",
        "scope": "real image/file composer, URI/GNOME/path paste, edit/reload, MCP images and file references, native messages and isolated clipboard; no models",
        "clipboard": "private fake Unix-socket owner; no desktop clipboard is accessed",
        "binaries": {name: sha256(path) for name, path in (
            ("ssmcp", args.bin_dir / "ssmcp"), ("ssmcpd", args.bin_dir / "ssmcpd"),
            ("vt_snapshot", decoder))},
        "sources": {path.name: sha256(path) for path in (
            Path(__file__), Path(__file__).with_name("e2e_composer.py"),
            Path(__file__).with_name("e2e_dashboard.py"), Path(__file__).with_name("e2e.py"))},
    }
    with tempfile.TemporaryDirectory(prefix="ssmcp-attachments-e2e-") as directory:
        harness = Attachments(Path(directory), args.bin_dir, decoder)
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
            for cleanup in (harness.close_frontends, harness.stop_clipboard, harness.cleanup):
                try:
                    cleanup()
                except Exception as error:
                    report["status"] = "failed"
                    report.setdefault("cleanup_errors", []).append(f"{type(error).__name__}: {error}")
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            if args.report:
                args.report.parent.mkdir(parents=True, exist_ok=True)
                args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    if report["status"] != "passed":
        raise SystemExit(1)
    print(f"\nPassed {len(report['checks'])} attachment checks in {report['duration_seconds']}s", flush=True)


if __name__ == "__main__":
    main()
