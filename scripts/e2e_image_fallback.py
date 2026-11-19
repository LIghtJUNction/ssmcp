#!/usr/bin/env python3
"""Verify attachment cards in owned PTYs that never answer graphics probes.

Use already-built candidate ssmcp/ssmcpd/vt_snapshot binaries. No build, model,
installed harness, real display, production endpoint or user history is used.
Writes a JSON report, the known source PNG and actual wide/narrow VT snapshots.
Native image pixels remain covered separately by e2e_native_images.py.
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
import tempfile
import time

from e2e_composer import DOWN, HOME, LEFT, RIGHT
from e2e_dashboard import sha256, text
from e2e_input_history import InputHistory, png_fixture


class ImageFallback(InputHistory):
    def __init__(self, directory, binary_dir, decoder, output):
        super().__init__(directory, binary_dir, decoder, output)
        self.color_binaries = self.binary_dir  # Existing wrapper removes NO_COLOR.
        self.plain_binaries = Path(binary_dir).resolve()
        self.snapshots = []

    def client_environment(self, label, **options):
        environment = super().client_environment(label, **options)
        for key in ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET", "XAUTHORITY",
                    "DBUS_SESSION_BUS_ADDRESS", "TMUX", "STY", "SSH_TTY",
                    "SSH_CONNECTION", "SSH_CLIENT", "KITTY_WINDOW_ID", "ZTTY_SOCKET"):
            environment.pop(key, None)
        # A terminal name alone must never establish native graphics support.
        # Frontend only decodes output; it sends no Kitty, DA or cell-size replies.
        environment["TERM_PROGRAM"] = "ztty"
        return environment

    def resize(self, front, rows, cols):
        front.resize(rows, cols)
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            front.pump(0.05)
            if self.ui("/display/rows") == rows and self.ui("/display/cols") == cols:
                return
        raise AssertionError("the client did not observe the owned PTY resize")

    def read_image(self, expected):
        response = self.response(
            "const result=await call(%s,'ssmcp.fs.read_image',{path:'fallback.png'});"
            "image(result); return result.structuredContent;" % json.dumps(self.client_id)
        )
        self.check(not self.messages(response), "reading the image invents no queued requirement")
        blocks = [block for block in response.get("content", []) if block.get("type") == "image"]
        self.check(len(blocks) == 1 and blocks[0]["mimeType"] == "image/png"
                   and base64.b64decode(blocks[0]["data"], validate=True) == expected,
                   "real fs.read_image returns the complete known PNG unchanged as MCP image content")
        metadata = response["structuredContent"]["result"]
        self.check(metadata["path"] == "fallback.png" and metadata["size"] == len(expected)
                   and metadata["sha256"] == hashlib.sha256(expected).hexdigest(),
                   "native image metadata identifies the exact original file bytes")

    def save_snapshot(self, front, name, shot=None):
        shot = shot if shot is not None else front.snapshot(cells=True)
        encoded = (json.dumps(shot, ensure_ascii=False, separators=(",", ":")) + "\n").encode()
        if len(encoded) > 2 * 1024 * 1024 or len(shot["rows"]) > 96:
            raise AssertionError("fallback snapshot exceeded its fixed artifact budget")
        for secret in [self.secret, *self.sensitive]:
            if secret and secret.encode() in encoded:
                raise AssertionError("refusing to save fixture credentials in a VT snapshot")
        filename = f"fallback-{name}.json"
        (self.output / filename).write_bytes(encoded)
        (self.output / f"fallback-{name}.txt").write_text(text(shot) + "\n", encoding="utf-8")
        self.snapshots.append({"name": filename, "sha256": hashlib.sha256(encoded).hexdigest(),
                               "bytes": len(encoded), "size": shot["size"]})

    def card(self, front, label, rows, cols, no_color):
        dimensions = "2 × 2 · Image attachment"
        shot = front.until(lambda value: dimensions in text(value)
                           and "Inline preview unavailable" in text(value),
                           f"the {label} attachment card", cells=True)
        indexes = [index for index, row in enumerate(shot["rows"]) if dimensions in row]
        self.check(shot["alternate_screen"] and shot["size"] == [rows, cols]
                   and len(indexes) == 1, f"{label}: original dimensions and attachment label fit the actual PTY")
        index = indexes[0]
        self.check(index > 0 and index + 1 < rows
                   and "Inline preview unavailable" in shot["rows"][index + 1],
                   f"{label}: the compact card keeps its explanation immediately after its dimensions")
        self.check("▀" not in text(shot) and "▀".encode() not in front.transcript
                   and not any(re.fullmatch(r"[ .,:;+=*#%@-]{8,}", row)
                               and len(row.strip()) >= 8 for row in shot["rows"]),
                   f"{label}: no half-block preview or ASCII pixel field fills the terminal")
        self.check(all(len(row) == cols for row in shot["cells"])
                   and all(not row[-1]["wide"] for row in shot["cells"]),
                   f"{label}: cell rows remain bounded and no wide glyph crosses the last column")
        if no_color:
            self.check(all(cell["foreground"] is None and cell["background"] is None
                           for row in shot["cells"][index - 1:index + 2]
                           for cell in row if cell["text"].strip()),
                       f"{label}: NO_COLOR leaves the entire attachment card uncolored")
        self.save_snapshot(front, label, shot)

    def graphics_actions(self, front):
        packets = re.findall(rb"\x1b_G(.*?)\x1b\\", bytes(front.transcript), re.DOTALL)
        actions = []
        for packet in packets:
            fields = dict(part.split(b"=", 1) for part in packet.split(b";", 1)[0].split(b",")
                          if b"=" in part)
            actions.append(fields.get(b"a"))
        self.check(all(action in (b"q", b"d") for action in actions),
                   "an unanswered graphics probe never produces an image upload or placement")
        return actions

    def scenario(self, label, expected, no_color):
        self.binary_dir = self.plain_binaries if no_color else self.color_binaries
        front = self.start_front("image-fallback-" + label)
        self.resize(front, 30, 120)
        member = self.public_call("collab", {"action": "join", "client_id": self.client_id,
                                           "name": "Image fixture", "role": "local fallback test"})
        self.sensitive.append(member["token"])
        self.agents(front)
        front.until(lambda shot: "Image fixture" in text(shot), "the real fixture member in Agents")
        roster = self.queue()["agents"]
        selected = 1 + next(index for index, row in enumerate(roster)
                            if row["agent_id"] == member["agent_id"])
        front.send(HOME + DOWN * selected + RIGHT)
        self.wait_state(front, lambda state: state["target"] == member["agent_id"],
                        "the exact registered recipient was not selected")
        draft = "FALLBACK 草稿界"
        front.send(draft)
        self.wait_state(front, lambda state: state["draft"]["text"] == draft, "the ordinary UTF-8 draft")
        front.send(LEFT * 2)
        state = self.wait_state(front, lambda value: value["draft"]["cursor"] < len(draft.encode()),
                                "the UTF-8 cursor did not move within the draft")
        composition = {key: state[key] for key in ("draft", "target", "editing")}
        self.read_image(expected)
        self.card(front, label + "-wide", 30, 120, no_color)
        self.resize(front, 24, 36)
        self.card(front, label + "-narrow", 24, 36, no_color)
        after = self.ui("/display/conversation")
        self.check({key: after[key] for key in composition} == composition,
                   f"{label}: image delivery and narrow resize preserve draft, UTF-8 cursor, target and editing state")
        actions = self.graphics_actions(front)
        if not no_color:
            self.check(b"q" in actions, "the colored PTY actually received a graphics probe without answering it")
        self.check(not self.queue()["queue"], f"{label}: image display and inspection submit no user request")
        front.send(b"\x01\x0b")
        self.wait_state(front, lambda value: not value["draft"]["text"], "clearing only the test-owned draft")
        final, restored = front.disconnect()
        self.check(not final["alternate_screen"] and restored == front.original_termios
                   and front.process.returncode == 0,
                   f"{label}: disconnect restores the original termios and leaves the alternate screen")
        self.graphics_actions(front)
        self.metrics[label] = {"transcript_bytes": len(front.transcript),
                               "transcript_sha256": hashlib.sha256(front.transcript).hexdigest(),
                               "graphics_queries": actions.count(b"q"), "capability_replies_sent": 0}
        front.close()

    def run(self):
        self.prepare()
        self.workspace = self.directory / "workspace"
        self.workspace.mkdir()
        subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "-q"], cwd=self.workspace,
                       env={**os.environ, **self.safe_git_env, "HOME": str(self.home)},
                       check=True, timeout=5, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        expected = png_fixture()
        (self.workspace / "fallback.png").write_bytes(expected)
        (self.output / "source.png").write_bytes(expected)
        self.metrics["source_image"] = {"width": 2, "height": 2, "bytes": len(expected),
                                        "sha256": hashlib.sha256(expected).hexdigest()}
        self.scenario("color", expected, False)
        self.scenario("no-color", expected, True)


def main():
    def interrupted(_signal, _frame):
        raise TimeoutError("image fallback E2E interrupted; cleaning up owned processes")

    signal.signal(signal.SIGTERM, interrupted)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True, help="immutable candidate binary directory")
    parser.add_argument("--decoder", type=Path, help="override candidate vt_snapshot path")
    parser.add_argument("--out", type=Path, required=True, help="report, PNG and bounded VT snapshots")
    args = parser.parse_args()
    binaries = args.bin_dir.resolve()
    decoder = args.decoder or binaries / "examples/vt_snapshot"
    if not args.decoder and not decoder.is_file():
        decoder = binaries / "vt_snapshot"
    decoder = decoder.resolve()
    executables = {"ssmcp": binaries / "ssmcp", "ssmcpd": binaries / "ssmcpd", "vt_snapshot": decoder}
    for executable in executables.values():
        if not executable.is_file() or not os.access(executable, os.X_OK):
            parser.error(f"missing candidate executable: {executable}")
    root = Path(__file__).resolve().parent.parent
    report = {"schema": "ssmcp.image-fallback-e2e.v1", "status": "failed",
              "repository_commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=root, text=True).strip(),
              "platform": platform.system(), "models_started": False, "production_contacted": False,
              "transport": "isolated OpenSSH/OAuth/native MCP/owned PTY/independent VT decoder",
              "scope": "unanswered graphics probes and NO_COLOR attachment cards; no native pixel claim",
              "binaries": {name: sha256(path) for name, path in executables.items()},
              "fixture_sha256": sha256(Path(__file__))}
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-image-fallback-e2e-") as directory:
        harness = ImageFallback(Path(directory), binaries, decoder, args.out)
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
                    harness.save_snapshot(harness.last_frontend, "failure")
                except (OSError, AssertionError) as snapshot_error:
                    report["snapshot_error"] = type(snapshot_error).__name__
            harness.diagnostics()
        finally:
            harness.close_frontends()
            harness.cleanup()
            report.update(checks=harness.checks, metrics=harness.metrics, snapshots=harness.snapshots,
                          duration_seconds=round(time.monotonic() - started, 3))
            (harness.output / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    if report["status"] != "passed":
        raise SystemExit(1)
    print(f"Passed {len(report['checks'])} image fallback checks in {report['duration_seconds']}s", flush=True)


if __name__ == "__main__":
    main()
