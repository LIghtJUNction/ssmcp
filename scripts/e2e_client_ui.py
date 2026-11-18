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
import selectors
import signal
import struct
import subprocess
import tempfile
import termios
import time

from e2e_dashboard import Decoder


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


def left_rows(shot):
    return [row.split("│", 1)[0] for row in shot["rows"]]


def prompt_start(shot):
    return next((index for index, row in enumerate(left_rows(shot))
                 if row.startswith("─ To") or row.startswith("─ Editing")), None)


def prompt_text(shot):
    start = prompt_start(shot)
    return "\n".join(left_rows(shot)[start + 1:-1]) if start is not None else ""


def body_text(shot):
    start = prompt_start(shot)
    return "\n".join(left_rows(shot)[1:start]) if start is not None else ""


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
        check('theme = "dark"' in initial_config and 'layout = "chat"' in initial_config,
              "new configuration defaults to the dark conversation layout")
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
            check("● Active" in text(shot) and "○ Idle" in text(shot)
                  and "❯" in prompt_text(shot) and "exit=0" in text(shot),
                  f"{name} layout shows observed activity and its bottom composer")
            if cols >= 120:
                check("ssmcp  /  activity" in text(shot),
                      f"{name} wide header retains the view title")
            if rows >= 30:
                check("cargo test --lib" in text(shot), f"{name} layout retains the command and result")
            check(not shot["hide_cursor"] and shot["cursor"][0] >= rows - 4,
                  f"{name} cursor belongs to the bottom composer")
            expected_background = [255, 255, 255] if theme == "light" else [16, 20, 28]
            check(shot["cells"][0][0]["background"] == expected_background,
                  f"{name} theme supplies the actual terminal background")
            save_shot(args.out, name, shot)

        preview = Preview(binary, decoder, config)
        try:
            initial = preview.until(lambda s: "ssmcp  /  activity" in text(s)
                                    and "cargo test --lib" in text(s)
                                    and "Message the agent" in prompt_text(s), "default activity and composer")
            check(not initial["hide_cursor"] and initial["bracketed_paste"]
                  and not initial["application_cursor"] and not initial["application_keypad"],
                  "default keyboard modes belong to the visible composer")
            check("Builder" in text(initial) and "Reviewer" in text(initial),
                  "preview sidebar labels its two sample agents")
            save_shot(args.out, "chat", initial)

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
            audit = preview.until(lambda s: "code audit" in s["rows"][0]
                                  and "#example-01" in text(s), "Ctrl+O source details")
            code_rows = [index for index, row in enumerate(left_rows(audit))
                         if any(token in row for token in ["const ", "command:", "print("])]
            left_width = audit["rows"][0].find("│")
            left_width = left_width if left_width >= 0 else audit["size"][1]
            colors = {str(cell["foreground"]) for index in code_rows
                      for cell in audit["cells"][index][:left_width] if cell["text"].strip()}
            check(len(colors) >= 3 and audit["hide_cursor"], "focused source audit is syntax highlighted")
            save_shot(args.out, "audit", audit)
            preview.send(b"\x1b[D")
            preview.until(lambda s: "1 runs" in text(s), "Left returning to audit list")
            preview.send(b"\x1b[B\x1b[A\r")
            preview.until(lambda s: "#example-01" in text(s), "Enter opening selected source")
            check(True, "audit arrows navigate and Enter opens source")
            preview.send(b"\x1b[<0;3;3M")
            preview.until(lambda s: "1 runs" in text(s), "left mouse returning to audit list")
            preview.send(b"\x1b[<2;3;3M")
            preview.until(lambda s: "#example-01" in text(s), "right mouse opening source")
            check(True, "mouse audit open and back preserve source selection")
            preview.send(b"\x0f")
            preview.until(lambda s: "activity" in s["rows"][0] and not s["hide_cursor"], "Ctrl+O returning to input")

            slash(preview, "/filter ssmcp-missing-filter-marker")
            preview.until(lambda s: "No matching log entries" in body_text(s), "unmatched text filter")
            check(True, "unmatched text filters show an empty-result state")
            slash(preview, "/filter cargo")
            filtered = preview.until(
                lambda s: "cargo test --lib" in body_text(s) and "exit=0" in body_text(s)
                and "Improve the client experience" not in body_text(s), "matching text filter")
            check("cargo test --lib" in body_text(filtered) and "exit=0" in body_text(filtered),
                  "matching commands retain their result during filtering")
            slash(preview, "/filter")
            preview.until(lambda s: "Improve the client experience" in body_text(s), "cleared filter")
            check(True, "clearing the text filter restores other log records")
            preview.send(b"\x1b[20~")
            preview.until(
                lambda s: "cargo test --lib" in body_text(s) and "exit=0" in body_text(s)
                and "Improve the client experience" not in body_text(s), "command category filter")
            preview.send(b"\x1b[20~")
            preview.until(lambda s: "No matching log entries" in body_text(s), "error category filter")
            check(True, "F9 selects command and error categories")
            slash(preview, "/filter --all")
            preview.until(lambda s: "Improve the client experience" in body_text(s), "category clear")

            preview.resize(18, 120)
            live = preview.until(lambda s: "Write a requirement below" in body_text(s)
                                 and prompt_start(s) == 13, "compact live activity")
            preview.send(b"wheel draft")
            preview.until(lambda s: "wheel draft" in prompt_text(s), "draft before direct scrolling")
            before_scroll = body_text(live)
            preview.send(b"\x1b[<64;3;2M")
            scrolled = preview.until(lambda s: body_text(s) != before_scroll, "wheel scroll without F6")
            check("wheel draft" in prompt_text(scrolled) and not scrolled["hide_cursor"],
                  "wheel scrolls activity directly while preserving composer text and focus")
            preview.send(b"\x1b[<64;3;2M" * 8)
            preview.until(lambda s: "Improve the client experience" in body_text(s), "retained activity history")
            preview.send(b"\x1b[1;5F")
            preview.until(lambda s: "Write a requirement below" in body_text(s), "Ctrl+End following live output")
            clear_draft(preview)
            preview.resize(30, 120)
            preview.until(lambda s: prompt_start(s) == 25, "restored full-height composer")

            slash(preview, "/agents")
            agents = preview.until(lambda s: "ssmcp  /  agents" in text(s)
                                   and "example-reviewer" in text(s), "sample agent roster")
            check("Next caller" in text(agents) and "Builder" in text(agents)
                  and "Reviewer" in text(agents) and agents["hide_cursor"],
                  "agents view separates the default recipient from sample participants")
            save_shot(args.out, "agents", agents)
            preview.send(b"\x1b[B\r")
            chosen = preview.until(lambda s: "To · Builder" in text(s) and not s["hide_cursor"], "explicit agent selection")
            check("example-builder" in text(chosen), "agent navigation selects the displayed participant")
            slash(preview, "/queue")
            queued = preview.until(lambda s: "example-request" in text(s) and "Requests ·" in text(s), "sample request queue")
            save_shot(args.out, "queue", queued)
            preview.send(b"\x1b[C")
            detail = preview.until(lambda s: "← back" in text(s)
                                   and "verify command output and source audit" in text(s), "queue request details")
            check(detail["hide_cursor"], "queue details use content focus without publishing data")
            preview.send(b"\x1b[D")
            preview.until(lambda s: "← back" not in text(s) and "example-request" in text(s), "queue list navigation")
            slash(preview, "/logs")
            preview.until(lambda s: "activity" in s["rows"][0], "activity slash command")

            preview.send(b"\x1bOR")
            preview.until(lambda s: "│" not in text(s) and "Builder" not in body_text(s), "collapsed sidebar")
            preview.send(b"\x1bOR")
            preview.until(lambda s: "│" in text(s) and "Reviewer" in text(s), "restored sidebar")
            check(True, "F3 collapses and restores the sidebar")

            preview.send(b"\x1bOP")
            preview.until(lambda s: "keyboard and commands" in text(s), "F1 help")
            preview.send(b"\x1b")
            preview.until(lambda s: "keyboard and commands" not in text(s), "help closed")
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
            slash(preview, "/logs")

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
