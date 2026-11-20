#!/usr/bin/env python3
"""Exercise local computer controls through two real PTYs in a private namespace.

Uses the existing vt_snapshot decoder and the account relay WebSocket fixture on
private loopback. No provider, model, external network or desktop input is used.
The outer runner never executes ssmcp outside bubblewrap.
"""

import argparse
import errno
import fcntl
import json
import os
from pathlib import Path
import re
import select
import shutil
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import uuid

from e2e_dashboard import Decoder, sha256
import e2e_account_relay


def save(path, value):
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(value, ensure_ascii=False, indent=2) + "\n")
    temporary.replace(path)


def render(snapshot, path, label):
    """Render only the actual terminal rows returned by the independent decoder."""
    from PIL import Image, ImageDraw, ImageFont

    font = None
    for candidate in ("DejaVuSansMono.ttf", "LiberationMono-Regular.ttf",
                      "/usr/share/fonts/TTF/DejaVuSansMono.ttf"):
        try:
            font = ImageFont.truetype(candidate, 16)
            break
        except OSError:
            pass
    if font is None:
        raise RuntimeError("PNG evidence needs an already-installed monospace font")
    rows, columns = snapshot["size"]
    cell, line, margin, top = font.getlength("M"), 22, 32, 76
    canvas = Image.new("RGB", (round(columns * cell + margin * 2), rows * line + top + 68), "#0f1115")
    drawing = ImageDraw.Draw(canvas)
    drawing.text((margin, 20), f"SSMCP / LIVE PTY / {label}", font=font, fill="#a7afb9")
    for index, row in enumerate(snapshot["rows"]):
        drawing.text((margin, top + index * line), row, font=font, fill="#d9dde4")
    if not snapshot["hide_cursor"]:
        row, column = snapshot["cursor"]
        x, y = margin + column * cell, top + row * line + line - 3
        drawing.rectangle((x, y, x + cell - 1, y + 1), fill="#d9dde4")
    drawing.text((margin, top + rows * line + 20),
                 "Decoded real PTY cells | private fixture | no desktop input or model invoked",
                 font=font, fill="#808894")
    canvas.save(path)


class Session:
    def __init__(self, root, label, binary, decoder, report):
        self.root = root / label
        self.root.mkdir()
        self.label, self.report = label, report
        bindir, workspace = self.root / "bin", self.root / "workspace"
        bindir.mkdir()
        workspace.mkdir()
        config = self.root / "config.toml"
        config.write_text("[ui]\ndashboard = true\n")
        self.state = e2e_account_relay.State()
        self.server = e2e_account_relay.ThreadingHTTPServer(("127.0.0.1", 0), e2e_account_relay.Handler)
        self.server.daemon_threads = True
        self.server.state = self.state
        self.state.origin = f"http://127.0.0.1:{self.server.server_address[1]}"
        self.server_thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.server_thread.start()
        env = os.environ.copy()
        env.update({"PATH": str(bindir), "TERM": "xterm-256color",
                    "SSMCP_AUTH_HOME": str(self.root / "auth"),
                    "SSMCP_CLIENT_CONFIG": str(config), "SSMCP_UI_CONFIG": str(config)})
        self.decoder = Decoder(decoder)
        self.decoder.request(op="resize", rows=38, cols=120)
        self.master, slave = os.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 38, 120, 0, 0))
        os.set_blocking(self.master, False)
        # Acquire only this new PTY in the child, then replace that process with
        # the real candidate. Avoid preexec_fn in the threaded HTTP fixture.
        tty_bootstrap = ("import fcntl,os,sys,termios; os.setsid(); "
                         "fcntl.ioctl(0,termios.TIOCSCTTY,0); os.tcsetpgrp(0,os.getpgrp()); "
                         "os.execv(sys.argv[1],sys.argv[1:])")
        self.process = subprocess.Popen(
            [sys.executable, "-c", tty_bootstrap, str(binary),
             "--server", self.state.origin, "--allow", self.state.allow_principal,
             "--name", label, "--no-watchdog", "--no-setup", "--no-browser", "--shell", "/bin/sh",
             "--ui-config", str(config)],
            cwd=workspace, env=env, stdin=slave, stdout=slave, stderr=slave)
        os.close(slave)
        self.transcript = (report / f"{label}.ansi").open("wb")
        self.screen = ""
        self.closed = False

    def pump(self):
        output = bytearray()
        while True:
            try:
                block = os.read(self.master, 65536)
                if not block:
                    break
                output.extend(block)
            except BlockingIOError:
                break
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                break
        if output:
            self.transcript.write(output)
            self.transcript.flush()
            self.decoder.feed(output)
            self.screen = "\n".join(self.decoder.request(op="snapshot", cells=False)["rows"])
        assert not self.state.errors, self.state.errors
        assert self.process.poll() is None, f"{self.label} exited: {self.screen[-3000:]}"

    def write(self, data):
        assert os.write(self.master, data) == len(data)

    def conversation(self, sessions):
        connections = until(sessions, lambda: self.state.connections.get(self.label),
                            f"{self.label} fixture connection")
        assert len(connections) == 1 and not connections[0].closed.is_set()
        result = connections[0].call("ssmcp.ui.get", {"path": "/display/conversation"})
        assert not self.state.errors, self.state.errors
        return result["value"]

    def capture(self, label):
        name = f"{self.label}-{label}"
        snapshot = self.decoder.request(op="snapshot", cells=True)
        data = self.report / f"{name}.json"
        preview = self.report / f"{name}.png"
        save(data, snapshot)
        render(snapshot, preview, name)
        return {"label": name, "source": "decoded real PTY bytes",
                "snapshot": data.name, "snapshot_sha256": sha256(data),
                "preview": preview.name, "preview_sha256": sha256(preview),
                "rows": snapshot["size"][0], "columns": snapshot["size"][1]}

    def close(self):
        if self.closed:
            return
        self.closed = True
        (self.report / f"{self.label}-screen.txt").write_text(self.screen)
        if self.process.poll() is None:
            os.killpg(self.process.pid, signal.SIGTERM)
            try:
                self.process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGKILL)
                self.process.wait(timeout=3)
        os.close(self.master)
        self.transcript.close()
        self.decoder.close()
        self.server.shutdown()
        self.server.server_close()
        self.server_thread.join(timeout=2)
        assert not self.state.errors, self.state.errors


def until(sessions, predicate, label, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        for session in sessions:
            session.pump()
        result = predicate()
        if result:
            return result
        select.select([session.master for session in sessions], [], [], 0.02)
    raise AssertionError(f"timed out: {label}")


def quiet(sessions, seconds=0.4):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        for session in sessions:
            session.pump()
        select.select([session.master for session in sessions], [], [], 0.02)


def badge(session):
    return set(re.findall(r"foreground (on|off) · background on", session.screen))


def inside(args):
    assert os.stat("/proc/self/ns/mnt").st_ino != args.parent_mountns, "a private mount namespace is required"
    mounts = Path("/proc/self/mountinfo").read_text().splitlines()
    assert any(line.split()[4] == "/var/tmp" and " - tmpfs " in line for line in mounts)
    assert not any(os.environ.get(key) for key in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS"))
    probe = Path("/var/tmp") / args.probe
    probe.write_text("private namespace only\n")
    gate = Path(f"/var/tmp/ssmcp-host-input-{os.geteuid()}")
    assert not gate.exists(), "the fixture must begin without a persisted gate"
    report = {"ok": False, "private_namespace": True, "transport": "loopback WebSocket",
              "fixture_sha256": sha256(Path(e2e_account_relay.__file__)), "binary_sha256": sha256(args.binary),
              "decoder_sha256": sha256(args.decoder), "cases": [], "badge_observations": [],
              "captures": []}
    sessions = []

    def cli(action="status"):
        result = subprocess.run([str(args.binary), "computer", action, "--json"],
                                capture_output=True, text=True, check=True, timeout=10)
        value = json.loads(result.stdout)
        assert value["background_enabled"] is True, value
        assert value["scope"] == "machine_os_user", value
        assert re.fullmatch(r"[0-9a-f]{32}", value["generation"]), value
        return value

    def check(name, **details):
        report["cases"].append({"name": name, "passed": True, **details})

    def capture(label):
        report["captures"].extend(session.capture(label) for session in sessions)

    def badges(enabled):
        expected = {"on" if enabled else "off"}
        started = time.monotonic()
        until(sessions, lambda: all(badge(s) == expected for s in sessions), f"both badges {expected}")
        observed_ms = round((time.monotonic() - started) * 1000, 2)
        value = cli()
        assert value["enabled"] is enabled, value
        report["badge_observations"].append({"enabled": enabled, "sessions": len(sessions),
                                             "observed_ms": observed_ms, "generation": value["generation"]})
        return value

    try:
        initial = cli()
        assert initial["enabled"] is True
        check("default_foreground_and_background_on")
        off = cli("off")
        assert off["enabled"] is False and cli()["generation"] == off["generation"]
        check("cli_off_persists_between_processes", generation=off["generation"])
        with tempfile.TemporaryDirectory(prefix="ssmcp-controls-e2e-") as temporary:
            root = Path(temporary)
            for label in ("first", "second"):
                sessions.append(Session(root, label, args.binary, args.decoder, args.report_dir))
            first, second = sessions
            badges(False)
            # The first dashboard frame may precede input::Mode setup. Never
            # let the outer line discipline translate the fixture's Enter.
            def input_ready():
                modes = [termios.tcgetattr(session.master) for session in sessions]
                return all(not mode[0] & (termios.ICRNL | termios.INLCR | termios.IGNCR)
                           and not mode[3] & (termios.ICANON | termios.ECHO) for mode in modes)
            until(sessions, input_ready, "both owned PTYs ready for literal input")
            check("two_independent_tuis_restore_persisted_off")
            first.write(b"/computer on\r")
            on = badges(True)
            assert on["generation"] != off["generation"]
            check("local_slash_on_updates_both_idle_badges")
            capture("slash-on")
            first.write(b"/computer status\r")
            until(sessions, lambda: "Foreground on" in first.screen and "Super+Alt+C" in first.screen,
                  "status command describes the state and shortcut")
            assert cli()["generation"] == on["generation"]
            check("slash_status_is_read_only_and_visible")
            first.write(b"keep this draft")
            quiet(sessions)
            draft = first.conversation(sessions)
            assert draft["draft"]["text"] == "keep this draft", draft
            assert cli()["generation"] == on["generation"]
            check("ordinary_typing_preserves_enabled_foreground")
            first.write(b"\x1b[99;11u")
            shortcut_off = badges(False)
            assert first.conversation(sessions)["draft"] == draft["draft"]
            first.write(b"\x1b[99;11:2u\x1b[99;11:2u\x1b[99;11:3u")
            quiet(sessions)
            assert cli()["generation"] == shortcut_off["generation"]
            assert first.conversation(sessions)["draft"] == draft["draft"]
            check("super_alt_c_press_toggles_once_repeat_release_preserve_draft")
            capture("shortcut-off")
            first.write(b"\x1b[99;11:1u")
            on = badges(True)
            assert on["generation"] != shortcut_off["generation"]
            first.write(b"\x01\x0b/aud\t")
            quiet(sessions)
            state = first.conversation(sessions)
            assert state["draft"]["text"] == "/audit " and state["focus"] == "input", state
            first.write(b"\t")
            quiet(sessions)
            assert first.conversation(sessions)["focus"] == "content"
            first.write(b"\t")
            quiet(sessions)
            state = first.conversation(sessions)
            assert state["focus"] == "input" and state["draft"]["text"] == "/audit ", state
            assert cli()["generation"] == on["generation"]
            check("tab_keeps_completion_then_focus_without_toggling")
            second.write(b"/computer off\r")
            off = badges(False)
            assert first.conversation(sessions)["draft"]["text"] == "/audit "
            check("second_session_off_updates_idle_first_and_keeps_draft")
            capture("second-session-off")
            for session in sessions:
                session.close()
            sessions.clear()
            assert cli()["generation"] == off["generation"] and cli()["enabled"] is False
            sessions.append(Session(root, "restarted", args.binary, args.decoder, args.report_dir))
            badges(False)
            check("off_survives_all_tui_exits_and_new_session")
            capture("persisted-off")
            for session in sessions:
                session.close()
            sessions.clear()
        assert sha256(args.binary) == report["binary_sha256"], "binary changed during the test"
        report["ok"] = True
    except Exception as error:
        report["error"] = str(error)
        raise
    finally:
        for session in sessions:
            session.close()
        report["passed_count"] = sum(case["passed"] for case in report["cases"])
        save(args.report_dir / "report.json", report)
        lines = ["Computer controls real-PTY validation",
                 f"Result: {'PASS' if report['ok'] else 'FAIL'}",
                 f"Actual passed checks: {report['passed_count']}",
                 f"Binary SHA-256: {report['binary_sha256']}",
                 "Private mount/network namespace; no desktop input or model invoked.", ""]
        lines.extend(f"PASS {case['name']}" for case in report["cases"])
        if "error" in report:
            lines.extend(["", f"Failure: {report['error']}"])
        lines.extend(["", "Decoded terminal captures:"])
        lines.extend(f"{item['label']}: {item['preview']} / {item['snapshot']}"
                     for item in report["captures"])
        (args.report_dir / "summary.txt").write_text("\n".join(lines) + "\n")
        print(json.dumps(report, ensure_ascii=False), flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--decoder", type=Path,
                        default=Path("/var/tmp/project-z-cargo-target/release/examples/vt_snapshot"))
    parser.add_argument("--report-dir", type=Path, required=True)
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--parent-mountns", type=int, help=argparse.SUPPRESS)
    parser.add_argument("--probe", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.inside:
        inside(args)
        return
    args.binary = args.binary.resolve(strict=True)
    args.decoder = args.decoder.resolve(strict=True)
    args.report_dir = args.report_dir.resolve()
    args.report_dir.mkdir(parents=True, exist_ok=True)
    assert not (args.report_dir / "report.json").exists(), "use a fresh report directory to retain prior evidence"
    bwrap = shutil.which("bwrap")
    assert bwrap, "bubblewrap is required before any ssmcp invocation"
    probe = "ssmcp-controls-private-" + uuid.uuid4().hex
    command = [bwrap, "--unshare-user", "--unshare-pid", "--unshare-net", "--die-with-parent",
               "--ro-bind", "/", "/", "--dev", "/dev", "--proc", "/proc",
               "--tmpfs", "/var/tmp", "--tmpfs", "/tmp",
               "--ro-bind", str(args.binary.parent), "/var/tmp/controls-bin",
               "--ro-bind", str(args.decoder), "/var/tmp/vt_snapshot",
               "--bind", str(args.report_dir), "/var/tmp/report"]
    for key in ("DISPLAY", "WAYLAND_DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "SSH_AUTH_SOCK",
                "SSMCP_PLAYWRIGHT_COMMAND", "SSMCP_PLAYWRIGHT_ARGS", "SSMCP_CONTEXT_ROOTS"):
        command += ["--unsetenv", key]
    for key, directory in (("XDG_STATE_HOME", "state"), ("XDG_CONFIG_HOME", "config"),
                           ("XDG_DATA_HOME", "data"), ("XDG_CACHE_HOME", "cache")):
        command += ["--setenv", key, "/tmp/controls-" + directory]
    command += [sys.executable, str(Path(__file__).resolve()), "--inside",
                "--parent-mountns", str(os.stat("/proc/self/ns/mnt").st_ino), "--probe", probe,
                "--binary", "/var/tmp/controls-bin/" + args.binary.name,
                "--decoder", "/var/tmp/vt_snapshot", "--report-dir", "/var/tmp/report"]
    with (args.report_dir / "runner.stdout").open("w") as stdout, (args.report_dir / "runner.stderr").open("w") as stderr:
        result = subprocess.run(command, stdout=stdout, stderr=stderr, timeout=180)
    assert not (Path("/var/tmp") / probe).exists(), "private namespace probe leaked to the host"
    path = args.report_dir / "report.json"
    print(path.read_text() if path.exists() else (args.report_dir / "runner.stderr").read_text())
    if result.returncode:
        raise SystemExit(result.returncode)


if __name__ == "__main__":
    main()
