#!/usr/bin/env python3
"""Exercise Linux file clipboard MIME handling in owned PTYs.

Uses the existing isolated SSH/OAuth/InputHistory fixture and a private PATH
wl-paste executable. Never connects to the desktop clipboard, starts a model,
or changes a shared frontend. Build candidates separately, then run:
  python3 scripts/e2e_file_paste.py --bin-dir target/debug --out /tmp/file-paste

File copies use current wire7 @references and the rooted native image reader.
"""
import argparse
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import random
import re
import signal
import stat
import struct
import subprocess
import tempfile
import time
import zlib

from e2e_dashboard import Frontend, sha256, text
from e2e_input_history import InputHistory, png_fixture


PASTE_READER = r"""#!/usr/bin/python3
import base64
import json
import os
from pathlib import Path
import sys
import time

root = Path(os.environ["SSMCP_E2E_FILE_CLIPBOARD"])
selection = json.loads((root / "selection.json").read_text(encoding="utf-8"))
if sys.argv[1:] == ["--list-types"]:
    print("\n".join(selection["payloads"]))
else:
    if len(sys.argv) != 4 or sys.argv[1:3] != ["--no-newline", "--type"]:
        raise SystemExit("unexpected clipboard helper arguments")
    mime = sys.argv[3]
    with (root / "reads.log").open("a", encoding="utf-8") as log:
        log.write(mime + "\n")
    if selection["gate"]:
        (root / "blocked").touch()
        deadline = time.monotonic() + 2.5
        while not (root / "release").exists():
            if time.monotonic() >= deadline:
                raise SystemExit("fixture gate was not released")
            time.sleep(0.005)
    sys.stdout.buffer.write(base64.b64decode(selection["payloads"][mime], validate=True))
"""


def large_png_fixture():
    """A deterministic real RGBA PNG larger than the 400 KiB read budget."""
    def chunk(kind, data):
        return (struct.pack(">I", len(data)) + kind + data
                + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))
    width, height = 640, 256
    pixels = random.Random(26).randbytes(width * height * 4)
    scanlines = b"".join(b"\x00" + pixels[row * width * 4:(row + 1) * width * 4]
                         for row in range(height))
    return (b"\x89PNG\r\n\x1a\n"
            + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(scanlines)) + chunk(b"IEND", b""))


class GraphicsFrontend(Frontend):
    """Answer graphics discovery inside one owned PTY and inspect its real wire bytes."""

    def __init__(self, *args, **kwargs):
        self.protocol_cursor = 0
        self.protocol_pending = bytearray()
        self.screen_cursor = (1, 1)
        self.probes = 0
        self.cell_queries = 0
        self.native_uploads = []
        self.native_placements = []
        self.native_deletions = []
        self.transfer = None
        super().__init__(*args, **kwargs)

    def pump(self, seconds=0.05):
        super().pump(seconds)
        self.protocol_pending.extend(self.transcript[self.protocol_cursor:])
        self.protocol_cursor = len(self.transcript)
        pending = self.protocol_pending
        while pending:
            starts = [(pending.find(marker), marker) for marker in
                      (b"\x1b_G", b"\x1b[16t", b"\x1b[c")]
            starts = [(offset, marker) for offset, marker in starts if offset >= 0]
            if not starts:
                self.observe_cursor(pending[:-16] if len(pending) > 16 else b"")
                if len(pending) > 16:
                    del pending[:-16]
                break
            offset, marker = min(starts)
            self.observe_cursor(pending[:offset])
            if marker == b"\x1b_G":
                end = pending.find(b"\x1b\\", offset + 3)
                if end < 0:
                    del pending[:offset]
                    if len(pending) > 8192:
                        raise AssertionError("one Kitty protocol chunk exceeds its 4096-byte payload bound")
                    break
                body = bytes(pending[offset + 3:end])
                del pending[:end + 2]
                self.graphics_command(body)
            else:
                del pending[:offset + len(marker)]
                if marker == b"\x1b[16t":
                    self.cell_queries += 1
                    self.send(b"\x1b[6;20;10t")
                else:
                    self.send(b"\x1b[?62;4c")

    def observe_cursor(self, data):
        positions = re.findall(rb"\x1b\[(\d+);(\d+)H", data)
        if positions:
            self.screen_cursor = tuple(int(value) for value in positions[-1])

    def graphics_command(self, body):
        header, separator, payload = body.partition(b";")
        values = dict(field.decode("ascii").split("=", 1) for field in header.split(b",") if b"=" in field)
        action = values.get("a")
        if action == "q":
            self.probes += 1
            self.send(("\x1b_Gi=" + values["i"] + ";OK\x1b\\").encode("ascii"))
        elif action == "t" or (action is None and "m" in values):
            if not separator or len(payload) > 4096 or len(payload) % 4:
                raise AssertionError("native upload violates the observed Kitty base64 chunk contract")
            if action == "t":
                if self.transfer is not None or values.get("f") != "100" or values.get("t") != "d":
                    raise AssertionError("native upload must be a complete direct-data PNG transfer")
                self.transfer = {"id": int(values["i"]), "data": bytearray(), "chunks": 0}
            if self.transfer is None:
                raise AssertionError("continuation arrived without a native upload")
            self.transfer["data"].extend(payload)
            self.transfer["chunks"] += 1
            if len(self.transfer["data"]) > 8 * 1024 * 1024:
                raise AssertionError("native image transfer exceeded the fixture's bounded receive buffer")
            if values.get("m") == "0":
                png = base64.b64decode(self.transfer["data"], validate=True)
                width, height, pixels = self.decode_rgba(png)
                self.native_uploads.append({
                    "id": self.transfer["id"], "width": width, "height": height,
                    "chunks": self.transfer["chunks"], "bytes": len(png),
                    "rgba_sha256": hashlib.sha256(pixels).hexdigest(),
                })
                self.transfer = None
        elif action == "p":
            self.native_placements.append({
                **{key: int(values[key]) for key in ("i", "p", "x", "y", "w", "h") if key in values},
                "row": self.screen_cursor[0], "col": self.screen_cursor[1],
                "columns": int(values.get("c", (int(values["w"]) + 9) // 10)),
                "rows": int(values.get("r", (int(values["h"]) + 19) // 20)),
            })
        elif action == "d":
            if "i" not in values or values.get("d") not in ("i", "I"):
                raise AssertionError("native cleanup must delete only the client's exact image IDs")
            self.native_deletions.append(int(values["i"]))

    @staticmethod
    def decode_rgba(png):
        if png[:8] != b"\x89PNG\r\n\x1a\n":
            raise AssertionError("native upload is not PNG data")
        offset, compressed, dimensions = 8, bytearray(), None
        while offset < len(png):
            length = struct.unpack(">I", png[offset:offset + 4])[0]
            kind = png[offset + 4:offset + 8]
            data = png[offset + 8:offset + 8 + length]
            checksum = struct.unpack(">I", png[offset + 8 + length:offset + 12 + length])[0]
            if checksum != zlib.crc32(kind + data) & 0xFFFFFFFF:
                raise AssertionError("native PNG chunk checksum mismatch")
            if kind == b"IHDR":
                width, height, depth, color, compression, filtering, interlace = struct.unpack(">IIBBBBB", data)
                if (depth, color, compression, filtering, interlace) != (8, 6, 0, 0, 0):
                    raise AssertionError("native PNG does not use bounded noninterlaced RGBA8")
                if not (0 < width <= 1024 and 0 < height <= 1024):
                    raise AssertionError("native PNG exceeds the renderer dimension limit")
                dimensions = (width, height)
            elif kind == b"IDAT":
                compressed.extend(data)
            offset += length + 12
            if kind == b"IEND":
                break
        if dimensions is None or offset != len(png):
            raise AssertionError("native PNG has incomplete framing")
        width, height = dimensions
        stride = width * 4
        raw = zlib.decompress(compressed)
        if len(raw) != (stride + 1) * height:
            raise AssertionError("native RGBA rows have the wrong length")
        previous, pixels = bytes(stride), bytearray()
        for y in range(height):
            start = y * (stride + 1)
            mode, row = raw[start], bytearray(raw[start + 1:start + stride + 1])
            if mode not in range(5):
                raise AssertionError("native PNG uses an invalid row filter")
            for i in range(stride):
                left = row[i - 4] if i >= 4 else 0
                above, upper_left = previous[i], previous[i - 4] if i >= 4 else 0
                if mode == 1:
                    prediction = left
                elif mode == 2:
                    prediction = above
                elif mode == 3:
                    prediction = (left + above) // 2
                elif mode == 4:
                    estimate = left + above - upper_left
                    distances = (abs(estimate - left), abs(estimate - above), abs(estimate - upper_left))
                    prediction = (left, above, upper_left)[distances.index(min(distances))]
                else:
                    prediction = 0
                row[i] = (row[i] + prediction) & 255
            pixels.extend(row)
            previous = row
        return width, height, pixels


class FilePaste(InputHistory):
    def __init__(self, directory, binary_dir, decoder, output):
        super().__init__(directory, binary_dir, decoder, output)
        self.clipboard = directory / "fake-clipboard"
        self.clipboard.mkdir(mode=0o700)
        self.select({"text/plain;charset=utf-8": b""})

    def client_environment(self, label, **options):
        environment = super().client_environment(label, **options)
        # Frontend expects PATH to be one private directory; never append host PATH.
        helper = Path(environment["PATH"]) / "wl-paste"
        helper.write_text(PASTE_READER, encoding="utf-8")
        helper.chmod(0o700)
        environment.update(
            WAYLAND_DISPLAY="ssmcp-owned-file-paste-fixture",
            SSMCP_E2E_FILE_CLIPBOARD=str(self.clipboard),
        )
        environment.pop("DISPLAY", None)
        return environment

    def select(self, payloads, *, gate=False):
        for name in ("blocked", "release"):
            (self.clipboard / name).unlink(missing_ok=True)
        selection = {
            "payloads": {mime: base64.b64encode(data).decode("ascii")
                         for mime, data in payloads.items()},
            "gate": gate,
        }
        temporary = self.clipboard / "selection.tmp"
        temporary.write_text(json.dumps(selection), encoding="utf-8")
        temporary.replace(self.clipboard / "selection.json")

    def reads(self):
        path = self.clipboard / "reads.log"
        return path.read_text(encoding="utf-8").splitlines() if path.exists() else []

    def wait_file(self, front, name):
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            front.pump(0.01)
            if (self.clipboard / name).exists():
                return
            if front.process.poll() is not None:
                break
        raise AssertionError("private clipboard helper did not reach " + name)

    def inputs(self):
        root = self.workspace / ".ssmcp" / "inputs"
        if not root.exists():
            return {}
        result = {}
        for path in sorted(root.rglob("*")):
            mode = path.lstat().st_mode
            content = sha256(path) if stat.S_ISREG(mode) else None
            result[str(path.relative_to(root))] = (mode, content)
        return result

    def wait_inputs(self, front, before):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.02)
            if self.inputs() == before:
                return
        raise AssertionError("the rejected import left private files or changed an earlier copy")

    def clear_draft(self, front):
        state = self.ui("/display/conversation")
        draft = state["draft"]["text"]
        self.check(state["draft"]["cursor"] == len(draft.encode("utf-8")),
                   "the fixture clears only a draft whose cursor is at its UTF-8 end")
        front.send(b"\x7f" * len(draft))
        self.wait_state(front, lambda current: not current["draft"]["text"],
                        "the owned draft did not clear")

    def unchanged_queue(self, target):
        state = self.ui("/display/conversation")
        self.check(state["target"] == target and not self.queue()["queue"],
                   "local file input preserves the selected recipient and never auto-queues")
        return state

    def references(self, draft):
        escaped = re.findall(r'@"((?:\\.|[^"\\])*)"', draft)
        self.check(draft == "\n" + "\n".join('@"' + item + '"' for item in escaped) + "\n",
                   "every imported file is one quoted reference between exact newlines")
        return [re.sub(r'\\([\\"])', r'\1', item) for item in escaped]

    def verify_copies(self, draft, originals):
        paths = self.references(draft)
        self.check(len(paths) == len(originals) and len(set(paths)) == len(paths),
                   "a successful MIME import inserts every selected file exactly once")
        expected = {path.name: path.read_bytes() for path in originals}
        self.check(len(expected) == len(originals), "fixture source names are distinct")
        actual = {}
        for index, relative in enumerate(paths, start=1):
            path = Path(relative)
            self.check(not path.is_absolute() and ".." not in path.parts
                       and len(path.parts) == 5 and path.parts[:2] == (".ssmcp", "inputs")
                       and re.fullmatch(r"files-[0-9a-f]{32}", path.parts[2])
                       and path.parts[3] == str(index),
                       "imported references identify only a private workspace file batch")
            copied = self.workspace / path
            self.check(stat.S_ISREG(copied.lstat().st_mode)
                       and stat.S_IMODE(copied.stat().st_mode) == 0o600
                       and stat.S_IMODE(copied.parent.stat().st_mode) == 0o700
                       and stat.S_IMODE(copied.parent.parent.stat().st_mode) == 0o700,
                       "imported files and their batch directory have private Unix permissions")
            actual[copied.name] = copied.read_bytes()
        self.check(actual == expected, "all imported names and complete original bytes are preserved")
        self.check(all(path.read_bytes() == expected[path.name] for path in originals),
                   "copying clipboard files leaves every source present and unchanged")
        return paths

    def read_native_image(self, relative, expected):
        response = self.response(
            "const r = await call(%s, 'ssmcp.fs.read_image', %s); "
            "image(r); return r.structuredContent;" % (
                json.dumps(self.client_id), json.dumps({"path": relative}),
            )
        )
        images = [item for item in response["content"] if item.get("type") == "image"]
        self.check(not self.messages(response) and len(images) == 1
                   and images[0]["mimeType"] == "image/png"
                   and base64.b64decode(images[0]["data"], validate=True) == expected,
                   "the imported PNG is actually returned as complete native MCP image content")
        metadata = response["structuredContent"]["result"]
        self.check(metadata["path"] == relative and metadata["size"] == len(expected)
                   and metadata["sha256"] == hashlib.sha256(expected).hexdigest(),
                   "native image metadata and image bytes identify the same private file")

    def paste_success(self, front, mime, payload, originals, *, command=False, thumbnail=False):
        before_reads = len(self.reads())
        payloads = {mime: payload}
        if thumbnail:
            payloads["image/png"] = png_fixture()
        self.select(payloads)
        front.send("/paste\r" if command else b"\x16")
        state = self.wait_state(front, lambda current: current["draft"]["text"].startswith('\n@"'),
                                "the actual file MIME did not insert private references")
        paths = self.verify_copies(state["draft"]["text"], originals)
        self.check(self.reads()[before_reads:] == [mime],
                   "the native clipboard adapter reads file MIME once, before offered thumbnails")
        self.check(state["draft"]["cursor"] == len(state["draft"]["text"].encode("utf-8")),
                   "file insertion leaves the cursor at the complete UTF-8 reference block end")
        self.unchanged_queue(self.target)
        return paths

    def direct_image(self, front):
        self.clear_draft(front)
        source = large_png_fixture()
        self.check(400 * 1024 < len(source) <= 16 * 1024 * 1024,
                   "the direct PNG clipboard fixture really exceeds the native image read budget")
        before_reads = len(self.reads())
        self.select({"image/png": source, "text/plain;charset=utf-8": b"ignored thumbnail text"},
                    gate=True)
        front.send(b"\x16")
        self.wait_file(front, "blocked")
        try:
            front.send(b"\r")
            state = self.wait_state(front, lambda current: current["draft"]["text"] == "",
                                    "the pending image draft changed before clipboard bytes arrived")
            self.check(not self.queue()["queue"] and state["target"] == self.target,
                       "Enter during an empty pending image import sends no attachment or empty message")
        finally:
            (self.clipboard / "release").touch()
        state = self.wait_state(front, lambda current: current["draft"]["text"].startswith('\n@"'),
                                "the direct image MIME did not become a private reference")
        paths = self.references(state["draft"]["text"])
        self.check(len(paths) == 1 and re.fullmatch(r"\.ssmcp/inputs/image-[0-9a-f]{32}\.png", paths[0]),
                   "direct image MIME becomes one private PNG reference with no inline payload")
        copied = self.workspace / paths[0]
        encoded = copied.read_bytes()
        width, height = struct.unpack(">II", encoded[16:24])
        self.check(stat.S_ISREG(copied.lstat().st_mode)
                   and stat.S_IMODE(copied.stat().st_mode) == 0o600
                   and stat.S_IMODE(copied.parent.stat().st_mode) == 0o700,
                   "the direct clipboard image is stored with private Unix permissions")
        self.check(encoded[:8] == b"\x89PNG\r\n\x1a\n" and encoded[25] == 6
                   and 0 < width < 640 and 0 < height < 256 and len(encoded) <= 400 * 1024,
                   "a real oversized clipboard PNG retains alpha and is resized within 400 KiB")
        self.check(self.reads()[before_reads:] == ["image/png"],
                   "the adapter requests the offered image MIME exactly once instead of its text fallback")
        self.read_native_image(paths[0], encoded)
        self.unchanged_queue(self.target)
        self.save_shot("direct-image", front, cells=True)

    def binary_bracketed_paste(self, front):
        self.clear_draft(front)
        # A legal private PNG chunk forces signal/control bytes and several stdin
        # reads without altering the image's actual pixels.
        data = b"\x00\x03\x11\x13\xff\x1b[18~" + bytes(range(256)) * 40
        kind = b"raWp"
        chunk = (struct.pack(">I", len(data)) + kind + data
                 + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))
        original = png_fixture()
        source = original[:-12] + chunk + original[-12:]
        self.check(4096 < len(source) < 400 * 1024 and b"\x03" in source,
                   "the raw PNG fixture crosses stdin read boundaries and contains literal Ctrl-C bytes")
        before_reads = self.reads()
        packet = b"\x1b[200~" + source + b"\x1b[201~"
        previous = 0
        for boundary in (6, 7, 8, 11, 14, 15, len(packet) - 3, len(packet)):
            self.send_stream(front, packet[previous:boundary])
            previous = boundary
        state = self.wait_state(front, lambda current: current["draft"]["text"].startswith('\n@"'),
                                "raw bracketed PNG bytes did not become a private image reference")
        paths = self.references(state["draft"]["text"])
        self.check(len(paths) == 1 and re.fullmatch(r"\.ssmcp/inputs/image-[0-9a-f]{32}\.png", paths[0]),
                   "raw image bytes enter the same private reference path as clipboard images")
        copied = self.workspace / paths[0]
        self.check(copied.read_bytes() == source and stat.S_IMODE(copied.stat().st_mode) == 0o600,
                   "raw PNG paste preserves every original byte, including controls, with private file permissions")
        self.check(self.reads() == before_reads and front.process.poll() is None,
                   "binary paste does not consult the clipboard or trigger the outer terminal interrupt")
        self.read_native_image(paths[0], source)
        self.unchanged_queue(self.target)
        self.save_shot("raw-png-paste", front, cells=True)

        self.clear_draft(front)
        draft = "保留原稿与收件人 界"
        front.send(draft)
        before = self.wait_state(front, lambda current: current["draft"]["text"] == draft,
                                 "the malformed PNG test draft did not arrive")
        files = self.inputs()
        self.send_stream(front, b"\x1b[200~" + source[:24] + b"\x1b[201~")
        front.until(lambda shot: "Local input failed:" in text(shot) and "image" in text(shot),
                    "a truncated PNG reports an image import error")
        self.check(self.unchanged_queue(self.target)["draft"] == before["draft"]
                   and self.inputs() == files,
                   "malformed PNG paste preserves the draft and recipient and creates no private file")

        self.clear_draft(front)
        literal = "普通文字\n/paste 保持原文 界"
        payload = b"\x1b[200~" + literal.encode("utf-8") + b"\x1b[201~"
        for offset in range(0, len(payload), 3):
            self.send_stream(front, payload[offset:offset + 3])
        self.wait_state(front, lambda current: current["draft"]["text"] == literal,
                        "fragmented UTF-8 bracketed paste did not remain exact text")
        self.check(self.inputs() == files and self.reads() == before_reads,
                   "ordinary pasted text keeps Unicode and newlines without importing files or running /paste")
        self.unchanged_queue(self.target)

    def rejected_batch(self, front, valid, invalid, marker, label):
        self.clear_draft(front)
        draft = "保留原稿 " + label + " 界"
        front.send(draft)
        before_state = self.wait_state(front, lambda current: current["draft"]["text"] == draft,
                                       "the rejection fixture draft was not visible")
        before_files, before_reads = self.inputs(), len(self.reads())
        self.select({"text/uri-list": (valid.as_uri() + "\r\n" + invalid.as_uri() + "\r\n").encode()})
        front.send(b"\x16")
        front.until(lambda shot: "Local input failed:" in text(shot) and marker in text(shot),
                    label + " produces a clear native import error")
        self.check(self.reads()[before_reads:] == ["text/uri-list"],
                   label + " exercised the actual file-list clipboard adapter")
        after = self.unchanged_queue(self.target)
        self.check(after["draft"] == before_state["draft"],
                   label + " preserves the complete Unicode draft and cursor without partial insertion")
        self.wait_inputs(front, before_files)
        self.check(self.inputs() == before_files,
                   label + " leaves no partial batch and preserves existing private copies")
        self.save_shot(label, front)

    def native_graphics(self):
        front = GraphicsFrontend(self, "native-graphics", self.config("native-graphics"), self.decoder,
                                 rows=28, cols=100)
        self.frontends.append(front)
        self.last_frontend = front
        self.client_id = front.client_id
        self.oauth_login(front.credentials)
        front.until(lambda shot: shot["alternate_screen"] and "To · next caller" in text(shot),
                    "the independent native-graphics composer")
        self.check(front.probes == 1 and front.cell_queries >= 1,
                   "native graphics starts only after the real PTY capability and cell-size queries")
        self.clear_draft(front)

        width, height = 400, 300
        rgb = bytes(channel for y in range(height) for x in range(width)
                    for channel in ((x * 17 + y) % 256, (y * 31 + x // 2) % 256, (x ^ y) % 256))
        rgba = b"".join(rgb[i:i + 3] + b"\xff" for i in range(0, len(rgb), 3))
        def chunk(kind, data):
            return (struct.pack(">I", len(data)) + kind + data
                    + struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))
        rows = b"".join(b"\x00" + rgb[y * width * 3:(y + 1) * width * 3] for y in range(height))
        source = (b"\x89PNG\r\n\x1a\n"
                  + chunk(b"IHDR", struct.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
                  + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))
        path = "native-graphics.png"
        (self.workspace / path).write_bytes(source)
        self.read_native_image(path, source)
        front.until(lambda _shot: bool(front.native_uploads and front.native_placements),
                    "a real native PNG upload and placement")
        upload = front.native_uploads[-1]
        image_id = upload["id"]
        self.check(upload["width"] == width and upload["height"] == height
                   and upload["rgba_sha256"] == hashlib.sha256(rgba).hexdigest()
                   and upload["chunks"] > 1,
                   "the terminal receives every original pixel through multiple real Kitty PNG chunks")
        self.check(all(1 <= item["row"] and item["row"] + item["rows"] - 1 <= 28
                       and 1 <= item["col"] and item["col"] + item["columns"] - 1 <= 100
                       for item in front.native_placements),
                   "every initial native placement fits the physical terminal bounds")
        self.save_shot("native-graphics", front, cells=True)

        deletions = len(front.native_deletions)
        self.command(front, "/help")
        front.until(lambda _shot: len(front.native_deletions) > deletions,
                    "hiding activity removes its native placements")
        self.check(image_id in front.native_deletions[deletions:],
                   "help removes only the owned image instead of painting over stale graphics")
        uploads = len(front.native_uploads)
        front.send(b"\x1b")
        front.until(lambda _shot: len(front.native_uploads) > uploads,
                    "closing help restores the retained native image")
        self.check(front.native_uploads[-1]["rgba_sha256"] == upload["rgba_sha256"],
                   "reopening activity restores the same retained pixels")

        placements = len(front.native_placements)
        front.resize(18, 48)
        front.until(lambda _shot: len(front.native_placements) > placements,
                    "resizing redraws bounded native placements")
        resized = front.native_placements[-1]
        self.check(resized["row"] + resized["rows"] - 1 <= 18
                   and resized["col"] + resized["columns"] - 1 <= 48,
                   "native graphics remain inside the smaller terminal after resize")

        uploads, deletions = len(front.native_uploads), len(front.native_deletions)
        front.send(f"\x1b_Gi={image_id};ENOSPC:owned fixture failure\x1b\\")
        front.until(lambda shot: len(front.native_deletions) > deletions
                    and any("▀" in row for row in shot["rows"]),
                    "a native graphics error selects readable cell fallback")
        self.read_native_image(path, source)
        front.until(lambda shot: any("▀" in row for row in shot["rows"]),
                    "subsequent image output remains visible after native failure")
        self.check(len(front.native_uploads) == uploads,
                   "native errors stop subsequent PNG encoding/upload while preserving image fallback")
        self.metrics["native_graphics"] = {
            "probes": front.probes, "cell_queries": front.cell_queries,
            "uploads": len(front.native_uploads), "placements": len(front.native_placements),
            "scoped_deletions": len(front.native_deletions),
            "rgba_sha256": upload["rgba_sha256"],
        }
        final, restored = front.disconnect()
        self.check(not final["alternate_screen"] and restored == front.original_termios,
                   "native-graphics shutdown restores the terminal and leaves no active image transport")

    def logs_interrupt(self):
        config = self.config("logs-interrupt")
        config.write_text(config.read_text().replace('layout="chat"', 'layout="logs"'))
        front = self.frontend("logs-interrupt", config)
        self.oauth_login(front.credentials)
        front.until(lambda shot: shot["alternate_screen"] and "ssmcp" in text(shot),
                    "the isolated logs layout")
        self.send_stream(front, b"\x1b[200~\x89PNG\r\n\x1a\n\x00\x03\xff\x1b[201~")
        front.pump(0.15)
        self.check(front.process.poll() is None,
                   "logs-layout bracketed paste retains literal control bytes without disconnecting")
        final, restored = front.disconnect()
        self.check(not final["alternate_screen"] and restored == front.original_termios,
                   "logs-layout Ctrl-C outside paste disconnects and restores exact termios")

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir(mode=0o700)
        subprocess.run(
            ["/usr/bin/git", "-c", "init.defaultBranch=main", "init", "-q"], cwd=self.workspace,
            env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=True, timeout=5,
        )
        sources = self.directory / "copied files"
        sources.mkdir(mode=0o700)
        note, png = sources / 'notes "中文" draft.txt', sources / "image 中文.png"
        note.write_bytes("完整中文材料\nsecond line\n".encode("utf-8"))
        png.write_bytes(png_fixture())
        source_bytes = {path: path.read_bytes() for path in (note, png)}
        front = self.start_front("file-paste")
        self.target = self.ui("/display/conversation")["target"]
        self.check(not self.queue()["queue"], "the owned frontend begins with no queued requests")

        paths = self.paste_success(
            front, "text/uri-list",
            ("# file manager selection\r\n" + note.as_uri() + "\r\n" + png.as_uri() + "\r\n").encode(),
            [note, png], thumbnail=True,
        )
        native_text = self.call("ssmcp.fs.read_text", {"path": paths[0], "max_bytes": 1024})
        self.check(native_text["text"].encode("utf-8") == source_bytes[note]
                   and native_text["size"] == native_text["bytes"] == len(source_bytes[note])
                   and native_text["offset"] == 0 and native_text["eof"]
                   and native_text["next_offset"] == len(source_bytes[note])
                   and native_text["sha256"] == hashlib.sha256(source_bytes[note]).hexdigest(),
                   "the native rooted text tool reads every original UTF-8 byte of an imported file")
        self.read_native_image(paths[1], source_bytes[png])
        self.save_shot("uri-list", front, cells=True)
        self.direct_image(front)
        self.binary_bracketed_paste(front)

        self.clear_draft(front)
        self.paste_success(front, "x-special/gnome-copied-files",
                           ("cut\n" + note.as_uri() + "\n").encode(), [note], command=True)
        self.check(note.exists() and note.read_bytes() == source_bytes[note],
                   "GNOME cut clipboard intent is copied without moving or deleting its source")
        self.save_shot("gnome-cut", front)

        self.clear_draft(front)
        before_files, before_reads = self.inputs(), len(self.reads())
        literal = str(note)
        self.select({"text/plain;charset=utf-8": literal.encode("utf-8")})
        front.send(b"\x16")
        literal_state = self.wait_state(front, lambda current: current["draft"]["text"] == literal,
                                        "a plain-text absolute path did not remain literal")
        self.check(literal_state["draft"]["cursor"] == len(literal.encode("utf-8"))
                   and self.inputs() == before_files
                   and self.reads()[before_reads:] == ["text/plain;charset=utf-8"],
                   "text/plain path-like contents remain exact text and create no private file copies")
        self.unchanged_queue(self.target)

        missing, fifo, bad = sources / "missing-file.txt", sources / "named-pipe", sources / "bad.png"
        os.mkfifo(fifo, mode=0o600)
        bad.write_bytes(png_fixture()[:24])
        self.rejected_batch(front, note, missing, "missing-file", "missing")
        self.rejected_batch(front, note, fifo, "regular file", "fifo")
        self.rejected_batch(front, note, bad, "image", "bad-png")
        self.check(stat.S_ISFIFO(fifo.lstat().st_mode) and not missing.exists()
                   and bad.read_bytes() == png_fixture()[:24],
                   "the named pipe and invalid source fixtures remain unchanged")

        self.clear_draft(front)
        initial, addition = "正在编辑 界", "，继续补充"
        front.send(initial)
        self.wait_state(front, lambda current: current["draft"]["text"] == initial,
                        "the async edit fixture draft did not arrive")
        before_files, before_reads = self.inputs(), len(self.reads())
        self.select({"text/uri-list": (note.as_uri() + "\n" + png.as_uri() + "\n").encode()}, gate=True)
        front.send(b"\x16")
        self.wait_file(front, "blocked")
        try:
            front.send(addition)
            edited = self.wait_state(front, lambda current: current["draft"]["text"] == initial + addition,
                                     "typing during clipboard preparation did not remain editable")
        finally:
            (self.clipboard / "release").touch()
        front.until(lambda shot: "Draft or recipient changed" in text(shot),
                    "a successful async import rejects its stale draft stamp")
        after = self.unchanged_queue(self.target)
        self.check(after["draft"] == edited["draft"]
                   and after["draft"]["cursor"] == len((initial + addition).encode("utf-8"))
                   and self.reads()[before_reads:] == ["text/uri-list"],
                   "async completion preserves later Unicode edits instead of inserting stale references")
        self.wait_inputs(front, before_files)
        self.check(self.inputs() == before_files,
                   "discarding an async file result rolls back every new private copy")
        self.check(all(path.exists() and path.read_bytes() == data for path, data in source_bytes.items()),
                   "all source files remain byte-for-byte unchanged after every clipboard scenario")
        self.save_shot("stale-draft", front, cells=True)

        final, restored = front.disconnect()
        self.check(not final["alternate_screen"] and restored == front.original_termios,
                   "Ctrl-C outside pasted data gracefully disconnects and restores the exact owned terminal mode")
        self.native_graphics()
        self.logs_interrupt()


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("file-paste E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--decoder", type=Path, help="defaults to BIN_DIR/examples/vt_snapshot")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--report", type=Path)
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("this fixture exercises the Linux wl-paste MIME adapter")
    decoder = args.decoder or args.bin_dir / "examples/vt_snapshot"
    binaries = {"ssmcp": args.bin_dir / "ssmcp", "ssmcpd": args.bin_dir / "ssmcpd", "vt_snapshot": decoder}
    for executable in binaries.values():
        if not executable.is_file():
            parser.error(f"missing {executable}; build the candidates and vt_snapshot first")
    args.out.mkdir(parents=True, exist_ok=True)
    started = time.monotonic()
    report = {
        "status": "failed", "platform": platform.system(),
        "transport": "isolated OpenSSH public-key + OAuth + owned PTY + independent vt100 decoder",
        "clipboard": "private PATH fake wl-paste; no real desktop clipboard or shared frontend",
        "scope": "raw PNG paste and exact MCP image readback, malformed input, UTF-8, Ctrl-C/termios restoration, private file batches, real Kitty probe/upload/pixel integrity/placement/hide/resize/error fallback; no models",
        "binaries": {name: sha256(path) for name, path in binaries.items()},
        "sources": {name: sha256(Path(__file__).with_name(name)) for name in (
            "e2e_file_paste.py", "e2e_input_history.py", "e2e.py", "e2e_dashboard.py", "e2e_composer.py",
        )},
    }
    with tempfile.TemporaryDirectory(prefix="ssmcp-file-paste-e2e-") as directory:
        harness = FilePaste(Path(directory), args.bin_dir, decoder, args.out)
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
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "duration_seconds": report["duration_seconds"]}), flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
