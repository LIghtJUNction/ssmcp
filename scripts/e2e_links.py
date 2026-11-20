#!/usr/bin/env python3
"""Actual compositor/input/opener contract in a private Linux PTY.

Run with the already-built lib test ELF and examples/vt_snapshot. No browser,
relay, model, installed client or user's terminal is contacted. Targets are
reserved .test hosts and a private xdg-open writes literal argv to a JSONL file.
"""
import argparse
import errno
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import selectors
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import time

from e2e_dashboard import Decoder


def digest(path):
    value = hashlib.sha256()
    with Path(path).open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(block)
    return value.hexdigest()


class Fixture:
    def __init__(self, binary, decoder, root):
        self.root = root
        self.sequence = 0
        self.transcript = bytearray()
        self.decoder = Decoder(decoder)
        self.decoder.request(op="resize", rows=30, cols=120)
        self.master, self.slave = os.openpty()
        self.original = termios.tcgetattr(self.slave)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 120, 0, 0))
        os.set_blocking(self.master, False)
        (root / "fixture.marker").write_text("private disposable fixture\n")
        bin_dir = root / "bin"
        bin_dir.mkdir()
        opener = bin_dir / "xdg-open"
        opener.write_text(f"#!{sys.executable}\nimport json,os,sys\nwith open(os.environ['SSMCP_LINK_OPEN_LOG'],'a') as f: f.write(json.dumps(sys.argv[1:])+chr(10))\n")
        opener.chmod(0o700)
        env = dict(os.environ, TERM="xterm-256color", PATH=f"{bin_dir}{os.pathsep}{os.environ.get('PATH', '')}",
                   SSMCP_LINK_PTY_FIXTURE=str(root), SSMCP_LINK_OPEN_LOG=str(root / "opened.jsonl"))
        env.pop("NO_COLOR", None)

        def own_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)
            os.tcsetpgrp(0, os.getpgrp())

        self.process = subprocess.Popen([
            str(binary), "--exact", "client::terminal::links::pty_fixture::links_physical_pty_fixture",
            "--ignored", "--nocapture", "--test-threads=1"],
            stdin=self.slave, stdout=self.slave, stderr=self.slave, env=env, preexec_fn=own_terminal)

    def pump(self, seconds=0.04):
        until = time.monotonic() + seconds
        data = bytearray()
        with selectors.DefaultSelector() as selector:
            selector.register(self.master, selectors.EVENT_READ)
            while time.monotonic() < until:
                if not selector.select(max(0, until - time.monotonic())):
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

    def shot(self):
        return self.decoder.request(op="snapshot", cells=True)

    def state(self):
        try:
            return json.loads((self.root / "state.json").read_text())
        except (FileNotFoundError, json.JSONDecodeError):
            return {}

    def until(self, predicate, label, timeout=8):
        until = time.monotonic() + timeout
        shot = None
        while time.monotonic() < until:
            self.pump()
            shot = self.shot()
            if predicate(shot, self.state()):
                return shot
            if self.process.poll() is not None:
                break
        raise AssertionError(f"did not observe {label}; exit={self.process.poll()}\n" + "\n".join((shot or {}).get("rows", [])))

    def send(self, data):
        os.write(self.master, data)

    def mouse(self, x, y, release=False, button=0):
        self.send(f"\x1b[<{button};{x+1};{y+1}{'m' if release else 'M'}".encode())

    def command(self, op, **kw):
        self.sequence += 1
        path = self.root / "control.next"
        path.write_text(json.dumps(dict(sequence=self.sequence, op=op, **kw)))
        path.replace(self.root / "control.json")
        self.until(lambda _, state: state.get("sequence") == self.sequence, op)
        self.pump(0.12)

    def seeded(self, mode="links"):
        self.command("seed", mode=mode)
        return self.until(lambda _, state: state.get("ready"), "completed seeded frame")

    def opened(self):
        path = self.root / "opened.jsonl"
        return [json.loads(row) for row in path.read_text().splitlines()] if path.exists() else []

    def expect_open(self, target, before):
        self.until(lambda _, __: len(self.opened()) > before, "literal native opener argv")
        self.pump(0.12)
        assert self.opened()[before:] == [[target]], self.opened()[before:]

    def no_open(self, before):
        self.pump(0.45)
        assert len(self.opened()) == before, self.opened()[before:]

    def click(self, point, target):
        before = len(self.opened())
        self.mouse(*point)
        self.mouse(*point, release=True)
        self.expect_open(target, before)

    def resize(self, cols, rows=30):
        self.decoder.request(op="resize", rows=rows, cols=cols)
        fcntl.ioctl(self.slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.kill(self.process.pid, signal.SIGWINCH)
        self.pump(0.2)

    def close(self):
        if self.process.poll() is None:
            self.send(b"\x03")
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGTERM)
                self.process.wait(timeout=5)
        self.pump(0.1)
        restored = termios.tcgetattr(self.slave) == self.original
        self.decoder.close()
        os.close(self.master)
        os.close(self.slave)
        return dict(exit_code=self.process.returncode, termios_restored=restored)


def point(shot, text):
    for y, row in enumerate(shot["cells"]):
        for x in range(len(row)):
            if "".join(cell["text"] for cell in row[x:]).startswith(text):
                return x, y
    raise AssertionError(f"visible text not found: {text}\n" + "\n".join(shot["rows"]))


def assert_underlined(shot, where):
    x, y = where
    assert shot["cells"][y][x]["underline"], (x, y, shot["cells"][y][x])


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--test-bin", type=Path, required=True)
    parser.add_argument("--decoder", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if platform.system() != "Linux":
        parser.error("private opener interception is implemented for Linux only")
    args.output.mkdir(parents=True, exist_ok=False)
    report = {"complete": False, "checks": [], "test_binary_sha256": digest(args.test_bin), "decoder_sha256": digest(args.decoder)}
    def save():
        (args.output / "summary.json").write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n")
    def check(name):
        report["checks"].append({"name": name, "passed": True})
        save()
    save()
    with tempfile.TemporaryDirectory(prefix="ssmcp-link-e2e-") as temporary:
        fixture = Fixture(args.test_bin.resolve(), args.decoder.resolve(), Path(temporary))
        try:
            shot = fixture.until(lambda screen, state: state.get("ready") and "LINK_LABEL" in "\n".join(screen["rows"]), "initial link frame")
            (args.output / "initial.json").write_text(json.dumps(shot, ensure_ascii=False))
            for label, target in [
                ("LINK_LABEL", "https://markdown.example.test/a_(b)"),
                ("https://bare.example", "https://bare.example.test/path?q=1"),
                ("OSC_LABEL", "https://osc.example.test/old"),
            ]:
                where = point(fixture.shot(), label)
                assert_underlined(fixture.shot(), where)
                fixture.click(where, target)
                check(f"single click opens exact {label} target")
            assert all("ssmcp-link:" not in row for row in fixture.shot()["rows"])
            check("internal target IDs and OSC payloads are never visible")

            where = point(fixture.shot(), "OSC_LABEL")
            before = len(fixture.opened())
            fixture.mouse(*where)
            fixture.command("redraw")
            fixture.mouse(*where, release=True)
            fixture.expect_open("https://osc.example.test/old", before)
            check("unrelated status redraw preserves a stable pressed link")

            for case in ["drag", "double", "wheel", "copy", "replace", "resize"]:
                fixture.resize(120)
                shot = fixture.seeded()
                where = point(shot, "OSC_LABEL")
                before = len(fixture.opened())
                fixture.mouse(*where)
                if case == "drag":
                    fixture.mouse(where[0] + 2, where[1], button=32)
                    fixture.mouse(*where, button=32)
                elif case == "double":
                    fixture.mouse(*where, release=True)
                    fixture.mouse(*where)
                elif case == "wheel":
                    fixture.mouse(*where, button=64)
                elif case == "copy":
                    fixture.send(b"\x1b[18~")
                    fixture.until(lambda _, state: state.get("selection"), "F7 native selection")
                elif case == "replace":
                    fixture.seeded("replacement")
                elif case == "resize":
                    fixture.resize(70)
                fixture.mouse(*where, release=True)
                fixture.no_open(before)
                assert fixture.state()["draft"] == "draft must survive 界"
                check(f"{case} cancels link activation and preserves draft")
                if case == "copy":
                    fixture.send(b"\x1b")
                    fixture.pump(0.2)

            fixture.resize(120)
            shot = fixture.seeded("anchored")
            where = point(shot, "LINK_LABEL")
            before = len(fixture.opened())
            fixture.mouse(*where)
            fixture.command("append")
            assert point(fixture.shot(), "LINK_LABEL") == where
            fixture.mouse(*where, release=True)
            fixture.expect_open("https://markdown.example.test/a_(b)", before)
            check("anchored background append preserves the same visible click target")

            # The separate vt100 decoder is scalar based and is not an oracle
            # for emoji shaping. Use fixed Unicode terminal cell coordinates:
            # 4 event cells + 2 Markdown margin + 2 emoji cells + one space.
            # Actual mouse bytes still traverse the real PTY and opener.
            fixture.resize(120)
            fixture.seeded("emoji")
            for label, suffix in [("FAMILY_LINK", "family"), ("HEART_LINK", "heart"), ("FLAG_LINK", "flag"), ("CODER_LINK", "coder")]:
                _, y = point(fixture.shot(), label)
                expected = f"https://emoji.example.test/{suffix}"
                hit = next(hit for hit in fixture.state()["links"] if hit["target"] == expected)
                assert hit["start"] == 9 and hit["row"] == y, hit
                fixture.click((9, y), expected)
                check(f"fixed cell 9 after {suffix} grapheme opens exact target")

            for width in [70, 24, 9]:
                fixture.resize(width)
                fixture.seeded()
                fixture.until(lambda _, state: state.get("ready") and state.get("links"), "wrapped visible targets")
                hit = fixture.state()["links"][-1]
                expected = "https://wrapped.example.test/long_path/with_many_parts/%E7%95%8C?q=1&value=two"
                assert hit["target"] == expected, hit
                where = hit["start"], hit["row"]
                assert_underlined(fixture.shot(), where)
                fixture.click(where, expected)
                check(f"{width}-column wrapped Unicode link opens its full original target")

            fixture.resize(120)
            shot = fixture.seeded("groups")
            where = point(shot, "fixture-command-7")
            before = len(fixture.opened())
            fixture.mouse(*where)
            fixture.mouse(*where, release=True)
            fixture.until(lambda _, state: state.get("events", {}).get("events", [{}])[-1].get("expanded"), "expanded grouped child")
            fixture.pump(0.2)
            fixture.no_open(before)
            check("tool heading still folds or opens without invoking a browser")
            output_point = point(fixture.shot(), "Command")
            fixture.mouse(2, output_point[1], button=65)
            fixture.mouse(2, output_point[1], button=65)
            fixture.until(lambda _, state: any(hit["target"].startswith("https://group.example.test/7/") for hit in state.get("links", [])), "inner tool output link")
            hit = next(hit for hit in fixture.state()["links"] if hit["target"].startswith("https://group.example.test/7/"))
            assert_underlined(fixture.shot(), (hit["start"], hit["row"]))
            fixture.click((hit["start"], hit["row"]), hit["target"])
            check("inner-scrolled tool output uses the visible child target")
            (args.output / "group.json").write_text(json.dumps(fixture.shot(), ensure_ascii=False))
            assert fixture.state()["draft"] == "draft must survive 界"
            check("all link and container gestures leave the composer draft intact")
        except Exception as error:
            report["error"] = str(error)
        finally:
            report["cleanup"] = fixture.close()
            (args.output / "terminal.ansi").write_bytes(fixture.transcript)
            report["opened"] = fixture.opened()
            report["complete"] = True
            report["ok"] = "error" not in report and report["cleanup"] == {"exit_code": 0, "termios_restored": True}
            save()
    print(json.dumps(report, ensure_ascii=False))
    return 0 if report["ok"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
