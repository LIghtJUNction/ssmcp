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
import struct
import subprocess
import tempfile
import termios
import time

from e2e_dashboard import Decoder


class Preview:
    def __init__(self, binary, decoder, config, rows=18, cols=80):
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
        while time.monotonic() < deadline:
            self.pump()
            shot = self.decoder.request(op="snapshot", cells=True)
            if predicate(shot):
                return shot
            if self.process.poll() is not None:
                break
        raise AssertionError(f"preview did not show {description}")

    def send(self, data):
        os.write(self.master, data)

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
        for name, theme, rows, cols in [("dark", "codex-dark", 36, 120),
                                         ("light", "light", 36, 120),
                                         ("narrow", "codex-dark", 18, 80)]:
            env = dict(os.environ)
            env.pop("NO_COLOR", None)
            frame = subprocess.check_output([
                str(binary), "ui", "preview", "--static", "--ui-config", str(config),
                "--theme", theme, "--rows", str(rows), "--cols", str(cols),
            ], env=env)
            vt = Decoder(decoder)
            try:
                vt.request(op="resize", rows=rows, cols=cols)
                vt.feed(frame)
                shot = vt.request(op="snapshot", cells=True)
            finally:
                vt.close()
            check("cargo test --lib" in text(shot) and "exit=0" in text(shot)
                  and "CODE_MODE" in text(shot), f"{name} layout shows command, result and audit")
            check(shot["hide_cursor"], f"{name} layout has no hidden Shell input cursor")
            (args.out / f"ui-{name}.json").write_text(json.dumps(shot, ensure_ascii=False))

        preview = Preview(binary, decoder, config)
        try:
            initial = preview.until(lambda s: "#example-01" in text(s), "source details")
            colors = {str(c["foreground"]) for row in initial["cells"][11:] for c in row
                      if c["text"].strip()}
            check(len(colors) >= 3, "source details contain multiple syntax colors")
            preview.send(b"\x1b[D")
            preview.until(lambda s: "1 runs" in text(s), "Left returning to audit list")
            preview.send(b"\x1b[B\x1b[A\r")
            preview.until(lambda s: "#example-01" in text(s), "Enter opening selected source")
            check(True, "audit arrows navigate and Enter opens source")

            preview.send(b"\x1b[<0;3;13M")
            preview.until(lambda s: "1 runs" in text(s), "left mouse returning to audit list")
            preview.send(b"\x1b[<2;3;13M")
            preview.until(lambda s: "#example-01" in text(s), "right mouse opening source")
            check(True, "mouse open and back preserve source selection")

            preview.send(b"/cargo\r")
            filtered = preview.until(lambda s: "Logs: all /cargo" in s["rows"][-1], "text filter")
            check("cargo test --lib" in text(filtered) and "exit=0" in text(filtered),
                  "matching commands retain their result during filtering")
            preview.send(b"\x1b")
            preview.until(lambda s: "Logs: all /cargo" not in s["rows"][-1], "cleared filter")
            preview.send(b"\x1b[20~\x1b[20~")
            preview.until(lambda s: "Logs: errors" in s["rows"][-1], "error category filter")
            check(True, "F9 selects command and error categories")
            preview.send(b"\x1b")
            preview.until(lambda s: "Logs: errors" not in s["rows"][-1], "category clear")

            preview.send(b"\x1b[<64;3;2M")
            preview.until(lambda s: "SSMCP / APPEARANCE" in text(s), "wheel scroll without F6")
            check(True, "wheel scrolls history without changing focus first")
            preview.send(b"\x1b[F")
            preview.until(lambda s: "[READY]" in text(s), "End following live output")

            preview.send(b"\x1bOP")
            preview.until(lambda s: "Help & appearance" in text(s), "F1 help")
            preview.send(b"\x1b")
            preview.until(lambda s: "Help & appearance" not in text(s), "help closed")
            preview.send(b"\x1bOS\x1b[19~")
            preview.until(lambda s: "Appearance saved" in text(s), "saved appearance")
            check('theme = "nord"' in config.read_text(), "F4 changes theme and F8 persists it")
            config.write_text('[ui]\nsidebar_width = 1\n')
            preview.send(b"\x1b[15~")
            preview.until(lambda s: "sidebar_width" in text(s), "invalid reload error")
            check(preview.process.poll() is None, "invalid reload keeps the active display alive")
        finally:
            restored = preview.close()
        check(restored, "preview restores the original terminal attributes")
        check(b"\x1b[?1000l\x1b[?1006l" in preview.transcript,
              "preview disables mouse reporting on exit")

    report = {"schema": "ssmcp.client-ui-smoke.v1", "ok": True,
              "checks": checks, "count": len(checks),
              "scope": "production compositor with sample events, isolated PTY; no live connection"}
    (args.out / "ui-smoke.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
