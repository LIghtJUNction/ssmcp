#!/usr/bin/env python3
"""Exercise the actual UI preview in a disposable PTY; no network or models.

Build `cargo build --bins --example vt_snapshot`, then pass --bin-dir.
Snapshots contain cells from the same VT decoder as the dashboard E2E.
"""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import re
import selectors
import signal
import struct
import subprocess
import tempfile
import termios
import time

from e2e_dashboard import Decoder


TOOL_GROUP = "Tools · 2 calls · done 2"
TOOL_COMMAND = "Ran cargo test --lib"
TOOL_DIFF = "src/client/terminal/events.rs"
COMMAND_SOURCE = "[RUN] #17  cargo test --lib"
COMMAND_CWD = "cwd  ~/project/ssmcp"
COMMAND_RESULTS = ("test client::ui::custom_colors ... ok", "test client::audit_view::browse ... ok")
DIFF_OLD = "- let expanded = true;"
DIFF_NEW = "+ let expanded = false;"
DIFF_HUNK = "@@ -1 +1 @@"


class Preview:
    def __init__(self, binary, decoder, config, rows=30, cols=120):
        self.decoder = Decoder(decoder)
        self.decoder.request(op="resize", rows=rows, cols=cols)
        self.master, self.slave = os.openpty()
        self.original = termios.tcgetattr(self.slave)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.set_blocking(self.master, False)
        self.transcript = bytearray()
        env = dict(os.environ, TERM="xterm-256color")
        env.pop("NO_COLOR", None)

        def own_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            os.tcsetpgrp(0, os.getpgrp())

        self.process = subprocess.Popen(
            [str(binary), "ui", "preview", "--ui-config", str(config)],
            stdin=self.slave, stdout=self.slave, stderr=self.slave,
            env=env, preexec_fn=own_terminal,
        )

    def pump(self, timeout=0.05):
        deadline = time.monotonic() + timeout
        data = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(self.master, selectors.EVENT_READ)
            while time.monotonic() < deadline:
                if not selector.select(max(0, deadline - time.monotonic())):
                    break
                try:
                    part = os.read(self.master, 65536)
                except OSError as error:
                    if error.errno in (errno.EIO, errno.EAGAIN):
                        break
                    raise
                if not part:
                    break
                data.extend(part)
        self.transcript.extend(data)
        self.decoder.feed(data)

    def until(self, predicate, description):
        deadline = time.monotonic() + 8
        shot = None
        while time.monotonic() < deadline:
            self.pump()
            shot = self.decoder.request(op="snapshot", cells=True)
            if predicate(shot):
                return shot
            if self.process.poll() is not None:
                break
        visible = text(shot) if shot else "no frame received"
        raise AssertionError(f"preview did not show {description}\n{visible}")

    def send(self, data):
        os.write(self.master, data)

    def resize(self, rows, cols):
        self.decoder.request(op="resize", rows=rows, cols=cols)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(self.process.pid, signal.SIGWINCH)

    def close(self):
        if self.process.poll() is None:
            self.send(b"\x03")
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.pump(0.1)
        restored = termios.tcgetattr(self.slave) == self.original
        self.decoder.close()
        os.close(self.master)
        os.close(self.slave)
        return restored


def text(shot):
    return "\n".join(shot["rows"])


def column(shot):
    # Read the final exact page tuple, independently of the project title.
    header = shot["rows"][0].split("│", 1)[0]
    matches = list(re.finditer(
        r"\b(agents|sessions|conversation|code audit|queue|questions|history|logs|tasks)\s+(\d+)/(\d+)\b",
        header, re.IGNORECASE,
    ))
    if matches:
        name, position, count = matches[-1].groups()
        return name.lower(), int(position), int(count)
    # The conversation header intentionally omits its numeric counter. Other
    # pages still expose their tuples, and the navigation checks verify all eight.
    if re.search(r"\bconversation\s*$", header, re.IGNORECASE):
        return "conversation", 2, 8
    return None


def view(shot, name):
    current = column(shot)
    return current is not None and current[0] == name and 1 <= current[1] <= current[2]


def logs_open(shot):
    return view(shot, "logs")


def left_rows(shot):
    divider = shot["rows"][0].find("│")
    return [row if divider < 0 else row[:divider] for row in shot["rows"]]


def prompt_start(shot):
    # The default recipient row has no label. Check the bottom composer itself.
    rows = shot["size"][0]
    if rows < 6:
        return None
    start = rows - (5 if rows >= 10 else 3)
    return start if left_rows(shot)[start + 1].lstrip().startswith("❯") else None


def prompt_text(shot):
    start = prompt_start(shot)
    return "\n".join(left_rows(shot)[start + 1:-1]) if start is not None else ""


def body_text(shot):
    start = prompt_start(shot)
    return "\n".join(left_rows(shot)[1:start]) if start is not None else ""


def folded_command_preview(shot):
    rows = body_text(shot).splitlines()
    headers = [index for index, row in enumerate(rows) if "Ran cargo test --lib" in row]
    if len(headers) != 1:
        return False
    header = headers[0]
    details = next((index for index in range(header + 1, len(rows))
                    if "+3 lines · Show details · Enter" in rows[index]), None)
    return (details is not None and details - header - 1 == 3
            and "Command completed successfully" in "\n".join(rows[header + 1:details])
            and "Source retained in the code audit" in "\n".join(rows[header + 1:details])
            and "test client::ui::custom_colors" not in body_text(shot)
            and "test client::audit_view::browse" not in body_text(shot))


def compact_tool_command(shot):
    body = body_text(shot)
    return (folded_command_preview(shot) and f"• {TOOL_COMMAND}" in body
            and "Command completed successfully." in body
            and "Source retained in the code audit." in body
            and re.search(r"\+(?:[1-9][0-9]* lines? ·)? Show details · Enter", body)
            and all(line not in body for line in (COMMAND_SOURCE, COMMAND_CWD, *COMMAND_RESULTS)))


def compact_tool_diff(shot):
    body = body_text(shot)
    return (f"▸ {TOOL_DIFF}" in body and DIFF_OLD in body and DIFF_NEW in body
            and DIFF_HUNK not in body)


def click_tool(preview, shot, marker, title):
    rows = [i for i, line in enumerate(left_rows(shot)[:prompt_start(shot)])
            if f"{marker} {title}" in line]
    assert len(rows) == 1, f"expected one visible {marker} {title} row, found {rows}"
    y = rows[0]
    cells = [x for x, cell in enumerate(shot["cells"][y][:len(left_rows(shot)[y])])
             if cell["text"] == marker and not cell["continuation"]]
    assert len(cells) == 1, f"expected one {marker} on row {y}, found {cells}"
    sgr = f"\x1b[<0;{cells[0] + 1};{y + 1}"
    preview.send(f"{sgr}M{sgr}m".encode())


def empty_composer_ready(shot):
    return (shot["alternate_screen"] and column(shot) == ("conversation", 2, 8)
            and "Message the agent" in prompt_text(shot) and not shot["hide_cursor"])


def clear_draft(preview):
    # Home/End and kill-line operate on logical lines, including Unicode input.
    # Four iterations cover the three-line draft exercised by this fixture.
    preview.send(b"\x05\x15\x7f" * 4)
    preview.until(lambda shot: "Message the agent" in prompt_text(shot), "empty input draft")


def slash(preview, command):
    preview.send(command.encode("utf-8") + b"\r")


def save_shot(directory, name, shot):
    (directory / f"ui-{name}.json").write_text(json.dumps(shot, ensure_ascii=False), encoding="utf-8")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", required=True, type=Path)
    parser.add_argument("--out", required=True, type=Path)
    args = parser.parse_args()
    binary = (args.bin_dir / "ssmcp").resolve()
    decoder = (args.bin_dir / "examples" / "vt_snapshot").resolve()
    args.out.mkdir(parents=True, exist_ok=True)
    checks = []

    def check(value, label):
        if not value:
            raise AssertionError(label)
        checks.append(label)

    with tempfile.TemporaryDirectory(prefix="ssmcp-ui-smoke-") as temp:
        config = Path(temp) / "config.toml"
        subprocess.run([str(binary), "ui", "init", "--ui-config", str(config)], check=True,
                       stdout=subprocess.DEVNULL)
        subprocess.run([str(binary), "ui", "check", "--ui-config", str(config)], check=True,
                       stdout=subprocess.DEVNULL)
        check(subprocess.run([str(binary), "ui", "init", "--ui-config", str(config)],
                             capture_output=True).returncode != 0,
              "init preserves an existing configuration")
        initial_config = config.read_text(encoding="utf-8")
        check('theme = "dark"' in initial_config and 'layout = "chat"' in initial_config
              and 'sidebar = false' in initial_config,
              "new configuration defaults to the dark conversation without a sidebar")
        for name, theme, rows, cols in [("dark", None, 36, 120),
                                      ("light", "light", 36, 120),
                                      ("narrow", None, 18, 80)]:
            env = dict(os.environ)
            env.pop("NO_COLOR", None)
            command = [
                str(binary), "ui", "preview", "--static", "--ui-config", str(config),
                "--rows", str(rows), "--cols", str(cols),
            ]
            if theme:
                command.extend(["--theme", theme])
            frame = subprocess.check_output(command, env=env, timeout=10)
            vt = Decoder(decoder)
            try:
                vt.request(op="resize", rows=rows, cols=cols)
                vt.feed(frame)
                shot = vt.request(op="snapshot", cells=True)
            finally:
                vt.close()
            save_shot(args.out, name, shot)
            check(column(shot) == ("conversation", 2, 8) and "❯" in prompt_text(shot)
                  and "done" in body_text(shot),
                  f"{name} layout shows the conversation column and its bottom composer")
            check("A quieter workspace" not in body_text(shot),
                  f"{name} conversation excludes background diagnostics")
            if rows >= 30:
                check(f"▾ {TOOL_GROUP}" in body_text(shot) and compact_tool_command(shot)
                      and compact_tool_diff(shot),
                      f"{name} layout shows an open command group and compact children")
            check(not shot["hide_cursor"] and shot["cursor"][0] >= rows - 4,
                  f"{name} cursor belongs to the bottom composer")
            expected_background = [255, 255, 255] if theme == "light" else [24, 24, 22]
            check(shot["cells"][0][0]["background"] == expected_background,
                  f"{name} theme supplies the actual terminal background")

        preview = Preview(binary, decoder, config)
        try:
            initial = preview.until(lambda s: column(s) == ("conversation", 2, 8)
                                    and f"▾ {TOOL_GROUP}" in body_text(s)
                                    and "Message the agent" in prompt_text(s),
                                    "default conversation column and composer")
            check(compact_tool_command(initial) and compact_tool_diff(initial),
                  "the default command group is open with both children compact")
            check(not initial["hide_cursor"] and initial["bracketed_paste"]
                  and not initial["application_cursor"] and not initial["application_keypad"],
                  "default keyboard modes belong to the visible composer")
            check("Reviewer" not in text(initial) and "Builder" not in text(initial),
                  "the default conversation keeps the sidebar hidden")
            account_row = left_rows(initial)[prompt_start(initial)]
            check("Local example" in account_row and "Max 1 GB" in account_row
                  and "Reset 12-01" in account_row,
                  "the composer shows its account, traffic limit and reset date")
            save_shot(args.out, "chat", initial)
            preview.send(b"\x1bOR")
            initial = preview.until(lambda s: "Builder" in text(s) and "Reviewer" in text(s)
                                    and "│" in text(s), "the explicitly opened sample-agent sidebar")
            check(column(initial) == ("conversation", 2, 8)
                  and "Message the agent" in prompt_text(initial),
                  "F3 exposes both sample agents while preserving the current page and composer")

            click_tool(preview, initial, "▾", TOOL_GROUP)
            folded = preview.until(lambda s: f"▸ {TOOL_GROUP}" in body_text(s)
                                   and TOOL_COMMAND not in body_text(s) and TOOL_DIFF not in body_text(s),
                                   "folded outer group hiding both children")
            save_shot(args.out, "group-folded", folded)
            click_tool(preview, folded, "▸", TOOL_GROUP)
            initial = preview.until(lambda s: f"▾ {TOOL_GROUP}" in body_text(s)
                                    and compact_tool_command(s) and compact_tool_diff(s),
                                    "reopened group retaining both compact children")
            check(True, "outer folding and reopening preserve bounded command output and the compact diff")
            save_shot(args.out, "group-reopened", initial)
            click_tool(preview, initial, "•", TOOL_COMMAND)
            expanded = preview.until(lambda s: COMMAND_SOURCE in body_text(s)
                                     and all(line in body_text(s) for line in COMMAND_RESULTS),
                                     "click-expanded command with its full results")
            check(f"• {TOOL_COMMAND}" in body_text(expanded)
                  and f"▾ {TOOL_GROUP}" in body_text(expanded)
                  and COMMAND_CWD in body_text(expanded) and "[OK] #17  exit=0" in body_text(expanded),
                  "expanded command retains its title, original working directory, results and exit status")
            check(sum(bool(line.strip()) for line in body_text(expanded).splitlines())
                  > sum(bool(line.strip()) for line in body_text(initial).splitlines()),
                  "explicit expansion reveals more content than the compact preview")
            save_shot(args.out, "command-expanded", expanded)
            preview.send(b"\x1b[6~")
            command_tail = preview.until(lambda s: COMMAND_SOURCE not in body_text(s)
                                         and "Source retained in the code audit." in body_text(s)
                                         and f"• {TOOL_COMMAND}" in body_text(s),
                                         "scrolling the expanded command to its retained tail")
            save_shot(args.out, "command-tail", command_tail)
            preview.send(b"\x1b[5~")
            command_head = preview.until(lambda s: COMMAND_SOURCE in body_text(s)
                                         and COMMAND_CWD in body_text(s),
                                         "scrolling the command back to its source")
            check(f"▾ {TOOL_GROUP}" in body_text(command_head),
                  "command details scroll independently inside the open group")
            click_tool(preview, command_head, "•", TOOL_COMMAND)
            collapsed = preview.until(lambda s: view(s, "conversation")
                                      and compact_tool_command(s) and compact_tool_diff(s),
                                      "click-collapsed command output returning to its result preview")
            click_tool(preview, collapsed, "▸", TOOL_DIFF)
            diff = preview.until(lambda s: view(s, "conversation")
                                 and f"▾ {TOOL_DIFF}" in body_text(s)
                                 and all(line in body_text(s) for line in (DIFF_OLD, DIFF_NEW, DIFF_HUNK)),
                                 "click-expanded diff with both source versions")
            check(compact_tool_command(diff),
                  "expanding a diff keeps the neighboring command at its compact preview")
            save_shot(args.out, "diff-expanded", diff)
            preview.send(b"\x1b")
            preview.until(lambda s: not s["hide_cursor"], "input focus after tool navigation")
            slash(preview, "/raw")
            raw = preview.until(lambda s: column(s) == ("logs", 7, 8)
                                and "test client::ui::custom_colors" in body_text(s)
                                and "A quieter workspace" in body_text(s),
                                "original output and diagnostics in the Logs column")
            save_shot(args.out, "logs", raw)
            check(all(line in body_text(raw) for line in (
                "cargo test --lib", COMMAND_CWD, "exit=0", *COMMAND_RESULTS,
                TOOL_DIFF, DIFF_HUNK, DIFF_OLD, DIFF_NEW,
            )), "raw Logs retain the full command, results and diff")
            slash(preview, "/raw")
            returned = preview.until(lambda s: column(s) == ("conversation", 2, 8)
                                     and f"▾ {TOOL_GROUP}" in body_text(s) and compact_tool_command(s)
                                     and f"▾ {TOOL_DIFF}" in body_text(s)
                                     and all(line in body_text(s) for line in (DIFF_OLD, DIFF_NEW, DIFF_HUNK)),
                                     "raw toggle restoring the open group and mixed child states")
            check("A quieter workspace" not in body_text(returned),
                  "/raw restores the open group and diff with the neighboring command compact")
            save_shot(args.out, "raw-restored", returned)
            click_tool(preview, returned, "▾", TOOL_DIFF)
            preview.until(lambda s: view(s, "conversation") and compact_tool_command(s)
                          and compact_tool_diff(s), "click-collapsed diff card")
            preview.send(b"\x1b")
            preview.until(lambda s: not s["hide_cursor"], "input focus after compact navigation")

            preview.send(b"q")
            typed = preview.until(lambda s: "❯ q" in prompt_text(s), "q as draft text")
            check(preview.process.poll() is None and not typed["hide_cursor"], "q types text instead of exiting")
            preview.send(b"\x7f")
            payload = "修复界面\n逐行核对 output".encode("utf-8")
            preview.send(b"\x1b[200~" + payload[:2])
            preview.pump(0.05)
            preview.send(payload[2:] + b"\x1b[201~")
            pasted = preview.until(lambda s: "修复界面" in prompt_text(s)
                                   and "逐行核对 output" in prompt_text(s), "UTF-8 multiline paste")
            check("Preview only" not in text(pasted) and "�" not in prompt_text(pasted),
                  "fragmented UTF-8 and pasted newlines stay in the draft without submission")
            preview.send(b"\nthird line")
            multiline = preview.until(lambda s: "third line" in prompt_text(s)
                                      and "修复界面" in prompt_text(s), "Ctrl+J third logical line")
            check("逐行核对 output" in prompt_text(multiline) and not multiline["hide_cursor"],
                  "Ctrl+J creates a visible multiline prompt")
            save_shot(args.out, "composer", multiline)
            preview.send(b"\r")
            retained = preview.until(lambda s: "Preview only" in text(s), "isolated preview submission notice")
            check("third line" in prompt_text(retained), "preview never publishes data and retains its draft")
            clear_draft(preview)

            preview.send(b"/re")
            preview.until(lambda s: "/reload" in prompt_text(s), "slash command suggestions")
            preview.send(b"\t\r")
            preview.until(lambda s: "Preview has no connected session" in text(s), "preview reload isolation")
            check(preview.process.poll() is None, "slash completion never restarts a real session in preview")
            clear_draft(preview)

            preview.send(b"\x0f")
            preview.until(lambda s: column(s) == ("code audit", 3, 8)
                          and "#example-01" in body_text(s)
                          and "Enter inspect" in body_text(s), "Ctrl+O audit list")
            preview.send(b"\r")
            audit = preview.until(lambda s: view(s, "code audit")
                                  and "#example-01" in body_text(s)
                                  and "command:" in body_text(s)
                                  and "print(" in body_text(s), "selected source details")
            code_rows = [index for index, row in enumerate(left_rows(audit))
                         if any(token in row for token in ["const ", "command:", "print("])]
            left_width = audit["rows"][0].find("│")
            left_width = left_width if left_width >= 0 else audit["size"][1]
            colors = {str(cell["foreground"]) for index in code_rows
                      for cell in audit["cells"][index][:left_width] if cell["text"].strip()}
            check(len(colors) >= 3 and audit["hide_cursor"], "focused source audit is syntax highlighted")
            save_shot(args.out, "audit", audit)
            preview.send(b"\x1b")
            listed = preview.until(lambda s: view(s, "code audit")
                                   and "1 runs" in body_text(s)
                                   and "Enter inspect" in body_text(s),
                                   "Escape returning to the audit list in its column")
            check(not listed["hide_cursor"], "Escape leaves the audit list with composer focus")
            preview.send(b"\x1b[Z\x1b[B\x1b[A\r")
            preview.until(lambda s: view(s, "code audit")
                          and "#example-01" in body_text(s)
                          and "command:" in body_text(s), "Enter opening selected source")
            check(True, "focused audit Up/Down navigates and Enter opens the selected source")
            preview.send(b"\x1b[<0;3;3M")
            preview.until(lambda s: view(s, "code audit")
                          and "1 runs" in body_text(s), "left mouse returning to audit list")
            preview.send(b"\r")
            preview.until(lambda s: view(s, "code audit")
                          and "#example-01" in body_text(s)
                          and "command:" in body_text(s), "reopening the same audit source")
            preview.send(b"\x1b[<2;3;3M\x1b[<2;3;3m")
            preview.until(lambda s: column(s) == ("logs", 7, 8)
                          and "A quieter workspace" in body_text(s), "right-click Logs from audit")
            preview.send(b"\x1b[<2;3;3M\x1b[<2;3;3m")
            restored_audit = preview.until(lambda s: column(s) == ("code audit", 3, 8)
                                          and "#example-01" in body_text(s)
                                          and "command:" in body_text(s),
                                          "right-click restoring the selected audit source")
            check(restored_audit["hide_cursor"],
                  "the Logs round trip preserves audit details and content focus")
            preview.send(b"\x0f")
            preview.until(lambda s: column(s) == ("conversation", 2, 8) and not s["hide_cursor"],
                          "Ctrl+O returning to the conversation composer")

            slash(preview, "/filter cargo test --lib")
            filtered = preview.until(lambda s: logs_open(s) and "· all /cargo test --lib" in s["rows"][0],
                                     "text filtering in the independent Logs column")
            check("cargo test --lib" in body_text(filtered) and "exit=0" in body_text(filtered)
                  and "A quieter workspace" not in body_text(filtered),
                  "matching commands retain their result while unrelated diagnostics are filtered out")
            slash(preview, "/filter")
            preview.until(lambda s: logs_open(s) and "/cargo" not in s["rows"][0]
                          and "A quieter workspace" in body_text(s), "cleared filter")
            preview.send(b"\x1b[20~")
            preview.until(lambda s: logs_open(s) and "· commands /" in s["rows"][0]
                          and "cargo test --lib" in body_text(s), "command category filter")
            preview.send(b"\x1b[20~")
            preview.until(lambda s: logs_open(s) and "· errors /" in s["rows"][0],
                          "error category filter")
            check(True, "F9 selects command and error categories")
            slash(preview, "/filter --all")
            preview.until(lambda s: logs_open(s) and "errors /" not in s["rows"][0]
                          and "A quieter workspace" in body_text(s), "category clear")

            slash(preview, "/logs")
            preview.until(lambda s: column(s) == ("logs", 7, 8), "idempotent Logs scrolling entry")
            preview.resize(18, 120)
            live = preview.until(lambda s: logs_open(s) and "Write a requirement below" in body_text(s)
                                 and prompt_start(s) == 13, "compact live diagnostic output")
            preview.send(b"wheel draft")
            preview.until(lambda s: logs_open(s) and "wheel draft" in prompt_text(s)
                          and not s["hide_cursor"], "shared draft edited inside Logs before scrolling")
            before_scroll = body_text(live)
            preview.send(b"\x1b[<64;3;2M")
            scrolled = preview.until(lambda s: logs_open(s) and body_text(s) != before_scroll
                                     and "rows below" in text(s)
                                     and "Ctrl+End to return to latest" in text(s),
                                     "wheel scroll and return-to-latest hint without F6")
            check("wheel draft" in prompt_text(scrolled) and not scrolled["hide_cursor"],
                  "wheel scrolls Logs directly while preserving composer text and focus")
            check("rows below" in text(scrolled) and "Ctrl+End to return to latest" in text(scrolled),
                  "scrolled output shows remaining rows and its return-to-latest shortcut")
            save_shot(args.out, "scroll-hint", scrolled)
            preview.send(b"\x1b[<64;3;2M" * 8)
            preview.until(lambda s: logs_open(s) and "Improve the client experience" in body_text(s),
                          "retained original transcript in Logs")
            preview.send(b"\x1b[1;5F")
            following = preview.until(lambda s: logs_open(s) and "Write a requirement below" in body_text(s)
                                      and "rows below" not in text(s),
                                      "Ctrl+End following live output")
            check("wheel draft" in prompt_text(following) and not following["hide_cursor"],
                  "Ctrl+End resumes following without changing the draft or composer focus")
            clear_draft(preview)
            preview.send(b"\x1b")
            preview.until(lambda s: column(s) == ("conversation", 2, 8),
                          "Escape returning from filtered Logs to the original conversation")
            preview.resize(30, 120)
            preview.until(lambda s: view(s, "conversation") and prompt_start(s) == 25,
                          "restored full-height conversation composer")

            slash(preview, "/agents")
            removed = preview.until(lambda s: "Unknown command" in text(s), "removed agent slash command")
            check(column(removed) == ("conversation", 2, 8) and "/agents" in prompt_text(removed),
                  "the removed command neither opens a view nor submits its text")
            clear_draft(preview)
            preview.send(b"\x1b[D")
            agents = preview.until(lambda s: column(s) == ("sessions", 1, 8)
                                   and "Reviewer" in body_text(s), "Left entering the adjacent Sessions column")
            check("Next caller" in body_text(agents) and "Builder" in body_text(agents)
                  and agents["hide_cursor"], "the agent panel shows selectable named recipients")
            save_shot(args.out, "agents", agents)
            preview.send(b"\x1b")
            returned = preview.until(lambda s: column(s) == ("conversation", 2, 8) and not s["hide_cursor"],
                                     "Escape returning to the default conversation")
            check("Message the agent" in prompt_text(returned), "closing agents restores the empty composer")
            preview.send("切换保留 界".encode() + b"\x01\x1b[D")
            preview.until(lambda s: column(s) == ("sessions", 1, 8)
                          and "切换保留 界" in prompt_text(s), "opening agents with a Unicode draft")
            preview.send(b"\x1b[B\r")
            chosen = preview.until(lambda s: "To Builder" in text(s)
                                   and column(s) == ("conversation", 2, 8) and not s["hide_cursor"],
                                   "explicit agent selection returning to the conversation")
            check("切换保留 界" in prompt_text(chosen)
                  and chosen["cursor"][0] >= chosen["size"][0] - 4,
                  "agent selection preserves the Unicode draft and returns cursor focus to the composer")
            clear_draft(preview)
            slash(preview, "/queue")
            queued = preview.until(lambda s: column(s) == ("queue", 4, 8)
                                   and "#example-" in body_text(s)
                                   and "Keep the interface simple;" in body_text(s)
                                   and "Requests ·" in body_text(s), "sample request queue column")
            save_shot(args.out, "queue", queued)
            preview.send(b"i")
            detail = preview.until(lambda s: view(s, "queue") and "Esc back" in body_text(s)
                                   and "ID: example-request" in body_text(s)
                                   and "verify command output and source audit" in body_text(s),
                                   "i inspecting queue request details")
            check(detail["hide_cursor"], "queue details use content focus without publishing data")
            preview.send(b"\x1b")
            preview.until(lambda s: view(s, "queue") and "Esc back" not in body_text(s)
                          and "#example-" in body_text(s)
                          and "Keep the interface simple;" in body_text(s), "Escape returning to the queue list")
            slash(preview, "/logs")
            preview.until(lambda s: column(s) == ("logs", 7, 8), "Logs opened from the queue column")
            preview.send(b"\x1b")
            queue_return = preview.until(lambda s: column(s) == ("queue", 4, 8)
                                         and "#example-" in body_text(s)
                                         and "Keep the interface simple;" in body_text(s),
                                         "Escape restoring the originating queue column")
            check("To Builder" in text(queue_return), "Logs preserves the shared recipient")
            preview.send(b"\x1b[1;3D\x1b[1;3D")
            preview.until(lambda s: column(s) == ("conversation", 2, 8)
                          and not s["hide_cursor"], "horizontal navigation back to the conversation")

            preview.send(b"\x1bOR")
            preview.until(lambda s: "│" not in s["rows"][0] and "Builder" not in body_text(s), "collapsed sidebar")
            preview.send(b"\x1bOR")
            preview.until(lambda s: "│" in text(s) and "Reviewer" in text(s), "restored sidebar")
            check(True, "F3 collapses and restores the sidebar")

            preview.send(b"\x1bOP")
            preview.until(lambda s: "ssmcp help" in text(s)
                          and "Write and send" in text(s), "F1 help")
            preview.send(b"\x1b[F")
            preview.until(lambda s: "Native harnesses" in text(s)
                          and "/pi, /cc, /codex" in text(s)
                          and "Backend capabilities and approvals stay with the installed harness." in text(s),
                          "full help scrolled to its final commands")
            preview.send(b"\x1b[H")
            preview.until(lambda s: "ssmcp help" in text(s)
                          and "Write and send" in text(s), "help returned to its beginning")
            check(True, "F1 exposes the full scrollable command help")
            preview.send(b"\x1b")
            preview.until(lambda s: "ssmcp help" not in text(s), "help closed")
            preview.send(b"\x1bOS\x1b[19~")
            preview.until(lambda s: "Appearance saved" in text(s), "saved appearance")
            check('theme = "codex-dark"' in config.read_text(encoding="utf-8"), "F4 cycles Dark to Codex Dark and F8 persists it")
            slash(preview, "/theme light")
            light = preview.until(lambda s: s["cells"][0][0]["background"] == [255, 255, 255], "named theme command")
            save_shot(args.out, "light-live", light)
            preview.send(b"\x1b[19~")
            preview.until(lambda s: "Appearance saved" in text(s)
                          and 'theme = "light"' in config.read_text(encoding="utf-8"), "saved named theme")
            saved_config = config.read_text(encoding="utf-8")
            check('theme = "light"' in saved_config, "slash theme changes appearance and F8 saves it")
            config.write_text('[ui]\nsidebar_width = 1\n', encoding="utf-8")
            preview.send(b"\x1b[15~")
            invalid = preview.until(lambda s: "sidebar_width" in text(s), "invalid appearance reload error")
            check(preview.process.poll() is None and invalid["cells"][0][0]["background"] == [255, 255, 255],
                  "invalid F5 configuration leaves the previous theme and display active")
            config.write_text(saved_config, encoding="utf-8")
            preview.send(b"\x1b[15~")
            preview.until(lambda s: column(s) == ("conversation", 2, 8)
                          and s["cells"][0][0]["background"] == [255, 255, 255]
                          and "sidebar_width" not in text(s),
                          "appearance reload preserving the conversation column")

            preview.resize(6, 40)
            preview.send("tiny界".encode("utf-8"))
            compact = preview.until(lambda s: prompt_start(s) == 3 and "tiny界" in prompt_text(s)
                                    and not s["hide_cursor"], "six-row composer")
            check(compact["cursor"][0] == 4 and compact["cursor"][1] < 40
                  and "�" not in prompt_text(compact), "small terminals keep a valid Unicode composer cursor")
            save_shot(args.out, "compact", compact)
            preview.resize(4, 32)
            preview.until(lambda s: s["hide_cursor"] and prompt_start(s) is None,
                          "safe four-row display without a hidden input cursor")
            preview.resize(30, 120)
            restored_view = preview.until(lambda s: prompt_start(s) == 25
                                          and "tiny界" in prompt_text(s) and not s["hide_cursor"], "draft after terminal growth")
            check("Reviewer" in text(restored_view), "resize preserves draft and restores available sidebar space")
            save_shot(args.out, "restored", restored_view)
        except Exception:
            preview.pump(0.1)
            save_shot(args.out, "failure", preview.decoder.request(op="snapshot", cells=True))
            raise
        finally:
            restored = preview.close()
        check(restored, "preview restores the original terminal attributes")
        check(preview.process.returncode == 0, "preview exits cleanly on Ctrl+C")
        check(b"\x1b[?1000l\x1b[?1006l" in preview.transcript,
              "preview disables mouse reporting on exit")

    report = {"schema": "ssmcp.client-ui-smoke.v2", "ok": True,
              "checks": checks, "count": len(checks),
              "scope": "production compositor with sample events, isolated PTY; no live connection"}
    (args.out / "ui-smoke.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
