#!/usr/bin/env python3
"""Exercise the real Linux desktop against an owned native account/session.

Uses prebuilt binaries, private Xvfb, actual X11 keyboard/mouse/clipboard and the
real production account router. No live user session, model or downloads.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import socket
import struct
import subprocess
import tempfile
import time
import traceback

from PIL import Image
from Xlib import X, display, xauth

from e2e import wait_for
from e2e_session_service import Sessions
from private_xvfb import PrivateXvfb


def identity(path):
    path = Path(path).resolve(strict=True)
    return {"path": str(path), "bytes": path.stat().st_size,
            "sha256": hashlib.sha256(path.read_bytes()).hexdigest()}


class DesktopInputs(Sessions):
    fixture_timeout_seconds = 300

    def __init__(self, directory, options, report):
        self.options, self.report = options, report
        self.fixture_binary = options.fixture_binary
        self.desktop = self.xvfb = self.xdisplay = None
        self.deliveries = []
        super().__init__(directory, options.bin_dir, options.report.parent)

    def client_environment(self, label):
        env = super().client_environment(label)
        owned = {"SSMCP_AUTH_HOME", "SSMCP_SESSION_HOME", "SSMCP_CLIENT_CONFIG", "SSMCP_UI_CONFIG"}
        return {key: value for key, value in env.items()
                if not key.startswith("SSMCP_") or key in owned}

    def code(self, code):
        outer = self.public_call("code_mode", {"code": code, "timeout_ms": 10000})
        messages = outer.get("user_messages", [])
        self.deliveries.extend(messages)
        return outer

    def native(self, name, arguments=None, observer=False):
        outer = self.code("return await call(%s, %s, %s);" % (
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments or {})))
        if observer and outer.get("user_messages"):
            raise AssertionError("an observation consumed the human requirement")
        result = outer.get("result", {})
        value = result.get("structuredContent")
        if result.get("isError") or not isinstance(value, dict) or value.get("spilled"):
            raise AssertionError(f"{name} did not return a complete native receipt: {result}")
        return value

    def queue(self):
        return self.native("ssmcp.ui.queue", {"action": "get"}, observer=True)["queue"]

    def xdo(self, *args):
        return subprocess.run(["/usr/bin/xdotool", *map(str, args)],
            env=self.gui_env, capture_output=True, text=True, check=True, timeout=8).stdout

    def key(self, *keys):
        self.xdo("key", "--clearmodifiers", *keys)
        time.sleep(0.12)

    def click(self, x, y):
        self.xdo("mousemove", "--window", self.window_id, int(x), int(y), "click", "1")
        time.sleep(0.12)

    def type(self, text):
        self.xdo("type", "--clearmodifiers", "--delay", "8", "--", text)
        time.sleep(0.12)

    def pixels(self):
        self.xdisplay.sync()
        geometry = self.window.get_geometry()
        data = self.window.get_image(0, 0, geometry.width, geometry.height, X.ZPixmap, 0xffffffff)
        return Image.frombytes("RGB", (geometry.width, geometry.height), data.data, "raw", "BGRX")

    def screenshot(self, name):
        path = self.artifact_dir / (name + ".png")
        self.pixels().save(path)
        self.report.setdefault("screenshots", []).append(identity(path))
        return path

    def clipboard(self):
        screen = self.xdisplay.screen()
        window = screen.root.create_window(0, 0, 1, 1, 0, screen.root_depth,
            X.InputOutput, X.CopyFromParent, event_mask=X.PropertyChangeMask)
        clip = self.xdisplay.intern_atom("CLIPBOARD")
        utf8 = self.xdisplay.intern_atom("UTF8_STRING")
        prop = self.xdisplay.intern_atom("SSMCP_DESKTOP_ACCEPTANCE_CLIPBOARD")
        try:
            window.convert_selection(clip, utf8, prop, X.CurrentTime)
            self.xdisplay.flush()
            deadline = time.monotonic() + 5
            while time.monotonic() < deadline:
                if not self.xdisplay.pending_events():
                    select.select([self.xdisplay.fileno()], [], [], max(0, deadline - time.monotonic()))
                while self.xdisplay.pending_events():
                    event = self.xdisplay.next_event()
                    if event.type == X.SelectionNotify and event.requestor.id == window.id:
                        if event.property == X.NONE:
                            raise AssertionError("the desktop clipboard owner rejected UTF-8")
                        value = window.get_full_property(prop, utf8)
                        if value is None:
                            raise AssertionError("the native clipboard property was absent")
                        return bytes(value.value).decode("utf-8")
            raise AssertionError("native clipboard transfer did not finish")
        finally:
            window.destroy()
            self.xdisplay.flush()

    def copied_draft(self):
        self.click(520, self.height - 92)
        self.key("ctrl+a", "ctrl+c")
        return self.clipboard()

    def fill_draft(self, first, second):
        self.click(520, self.height - 92)
        self.key("ctrl+a", "BackSpace")
        self.type(first)
        self.key("Return")
        self.type(second)

    def wait_record(self, text):
        def observed():
            rows = [row for row in self.queue() if row.get("text") == text]
            if not rows:
                return None
            if len(rows) != 1:
                raise AssertionError("a single desktop submission produced multiple local records")
            return rows[0]
        return wait_for(observed, timeout=8, description="original desktop queue receipt")

    def deliver(self, record, expected):
        before = len(self.deliveries)
        self.native("ssmcp.exec.list")
        messages = self.deliveries[before:]
        self.check(len(messages) == 1 and messages[0].get("message_id") == record["id"]
                   and messages[0].get("text") == expected,
                   "ordinary desktop input reaches the original HTTP user-message boundary exactly once")
        self.native("ssmcp.exec.list")
        self.check(len(self.deliveries) == before + 1,
                   "later work observation does not replay the delivered desktop requirement")
        return messages[0]

    def input_journal(self):
        cursor = generation = end = None
        inputs = []
        for _ in range(32):
            args = {"action": "thread", "client_id": self.client_id, "limit": 8}
            if cursor is not None:
                args.update(cursor=cursor, generation=generation)
            page = self.native("ssmcp.ui.events", args, observer=True)
            if generation is None:
                generation, end = page["generation"], page["end_cursor"]
            if page["generation"] != generation or page.get("retention_gap_possible"):
                raise AssertionError("original desktop journal changed generation or lost evidence")
            for row in page["events"]:
                if row["sequence"] < end and row.get("input_side") is True:
                    inputs.append(row)
            next_cursor = page["next_cursor"]
            if next_cursor >= end:
                return inputs
            if cursor is not None and next_cursor <= cursor:
                raise AssertionError("native desktop journal cursor failed to advance")
            cursor = next_cursor
        raise AssertionError("native desktop journal exceeded its finite read bound")

    def input_bands(self):
        image = self.pixels()
        # The production cards end 24 px from the window's right edge.
        sample_x = self.width - 36
        ys = [y for y in range(122, self.height - 140)
              if image.getpixel((sample_x, y)) == (38, 54, 80)]
        bands = []
        for y in ys:
            if not bands or y - bands[-1][-1] > 2:
                bands.append([y])
            else:
                bands[-1].append(y)
        return [(band[0], band[-1]) for band in bands if band[-1] - band[0] >= 50]

    def run_inputs(self):
        self.prepare()
        self.launch_manager()
        self.create("desktop-input-fixture", control=True)
        status = self.connected("desktop-input-fixture")
        self.authenticate_manager(status)
        names = {tool["name"] for tool in self.native_catalog(self.client_id)}
        self.check({"ssmcp.ui.queue", "ssmcp.ui.events", "ssmcp.ui.message", "ssmcp.exec.list"} <= names,
                   "the owned native session advertises the exact exercised UI and observation tools")
        baseline = self.native("ssmcp.exec.list")["jobs"]
        self.check(len(baseline) == 1 and baseline[0]["job_id"] == "main",
                   "the desktop fixture starts with only the original interactive shell")
        self.report["baseline_shell"] = {key: baseline[0].get(key) for key in ("job_id", "pid", "command")}
        self.xvfb = PrivateXvfb(self.directory / "desktop-xvfb", 1400, 1400).start()
        self.gui_env = {**self.env, **{key: value for key, value in self.xvfb.environment.items()
                       if key in {"DISPLAY", "XAUTHORITY", "ZUSE_BACKEND", "XDG_SESSION_TYPE"}}}
        for key in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "HYPRLAND_INSTANCE_SIGNATURE"):
            self.gui_env.pop(key, None)
        os.environ["DISPLAY"] = self.gui_env["DISPLAY"]
        # Python Xlib requires an exact FamilyLocal entry; libX11 accepts the
        # fixture's FamilyWild entry. Retain the SAME owned display credential.
        authority = xauth.Xauthority(self.gui_env["XAUTHORITY"])
        entries = [entry for entry in authority.entries if entry[2] == str(self.xvfb.number).encode()
                   and entry[3] == b"MIT-MAGIC-COOKIE-1"]
        if len(entries) != 1:
            raise AssertionError("the owned Xvfb credential is not unambiguous")
        local_authority = self.directory / "python-Xauthority"
        fields = (socket.gethostname().encode(), entries[0][2], entries[0][3], entries[0][4])
        encoded = struct.pack("!H", xauth.FamilyLocal) + b"".join(
            struct.pack("!H", len(field)) + field for field in fields)
        fd = os.open(local_authority, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "wb") as stream:
            stream.write(encoded)
        os.environ["XAUTHORITY"] = str(local_authority)
        self.desktop = self.spawn("desktop-window", [str(self.options.desktop_binary),
            "--session", "desktop-input-fixture", "--control", "--exit-after-ms", "60000",
            "--first-frame-receipt", str(self.artifact_dir / "first-frame.json"),
            "--frame-receipts", str(self.artifact_dir / "frames.jsonl")], env=self.gui_env)
        self.report["desktop_pid"] = self.desktop.pid
        def find_window():
            if self.desktop.poll() is not None:
                raise AssertionError("the original desktop process exited before its window appeared")
            result = subprocess.run(["/usr/bin/xdotool", "search", "--onlyvisible", "--name", "ssmcp-desktop"],
                env=self.gui_env, capture_output=True, text=True, timeout=3)
            values = result.stdout.split()
            if len(values) > 1:
                raise AssertionError("the private display unexpectedly contains multiple desktop windows")
            return values[0] if values else None
        self.window_id = wait_for(find_window, timeout=10, description="owned desktop X11 window")
        self.width, self.height = 1180, 1200
        self.xdo("windowsize", self.window_id, self.width, self.height)
        self.xdo("windowfocus", "--sync", self.window_id)
        self.xdisplay = display.Display(self.gui_env["DISPLAY"])
        self.window = self.xdisplay.create_resource_object("window", int(self.window_id))
        wait_for(lambda: self.pixels().size == (self.width, self.height)
                 and self.pixels().getpixel((self.width - 97, 29)) == (182, 206, 255),
                 timeout=10, description="rendered desktop with actual native control lease")
        self.screenshot("desktop-connected")
        self.check(json.loads((self.artifact_dir / "first-frame.json").read_text())["pid"] == self.desktop.pid,
                   "the actual window has a present receipt from the original desktop PID")
        initial = "Desktop human input\nsecond line remains text"
        self.fill_draft(*initial.split("\n"))
        self.check(self.copied_draft() == initial,
                   "real keyboard Enter and clipboard preserve the exact multiline desktop draft")
        self.check(self.queue() == [], "Enter inserts a newline without submitting human input")
        self.key("ctrl+Return")
        human_record = self.wait_record(initial)
        self.deliver(human_record, initial)
        self.report["human_delivery"] = {"record": human_record, "text": initial}
        generated = []
        for origin, body in [
            ("system_hook", "System hook: continue the original task"),
            ("agent_timer", "Timer prompt: the original task is still running"),
            ("worker_event", "Worker event: original check completed"),
            ("maintenance", "Maintenance: inspect this note\n@fixture-missing.txt $fixture_skill /reload"),
        ]:
            receipt = self.native("ssmcp.ui.message", {"text": body, "kind": "input",
                "input_origin": origin, "idempotency_key": "desktop_fixture_" + origin})
            self.check(receipt.get("accepted") is True and receipt.get("recorded") is True
                       and receipt.get("kind") == "input" and receipt.get("input_origin") == origin
                       and receipt.get("completion_permission") is False and bool(receipt.get("message_id")),
                       "generated desktop Input has its actual native receipt and passive origin: " + origin)
            generated.append({"origin": origin, "text": body, "receipt": receipt})
        self.report["generated_input_receipts"] = generated
        journal = self.input_journal()
        self.check({row["input_origin"] for row in journal} >=
                   {"human", "system_hook", "agent_timer", "worker_event", "maintenance"},
                   "the actual process journal contains human and all four generated input origins")
        for item in generated:
            matching = [row for row in journal if row.get("message_id") == item["receipt"]["message_id"]]
            self.check(len(matching) == 1 and matching[0]["text"] == item["text"],
                       "the desktop source journal contains the exact original native message once")
        self.report["input_journal"] = journal
        # Real bridge observations refresh without resending the seeded Inputs.
        bands = wait_for(lambda: self.input_bands() if len(self.input_bands()) >= 5 else None,
                         timeout=8, description="five actual right-side Input cards")
        self.report["input_card_pixel_bands"] = bands
        image = self.pixels()
        self.check(all(image.getpixel((40, (start + end) // 2)) != (38, 54, 80)
                       for start, end in bands),
                   "all five visible input cards leave a common left margin and align on the right")
        self.screenshot("desktop-five-input-origins")
        last = generated[-1]
        draft = "Preserved draft alpha\nPreserved draft beta"
        self.fill_draft(*draft.split("\n"))
        self.check(self.copied_draft() == draft, "the first verified send cleared only its original draft")
        self.key("Right")
        self.click(600, sum(bands[-1]) // 2)
        self.key("ctrl+shift+r")
        expected = "> 回复消息 " + last["receipt"]["message_id"] + "\n> " + last["text"].replace("\n", "\n> ") + "\n\n" + draft
        self.check(self.copied_draft() == expected,
                   "actual Ctrl+Shift+R quotes the selected original ID and preserves the multiline draft")
        self.check(len(self.queue()) == 1, "quoting with the shortcut does not submit another requirement")
        self.fill_draft(*draft.split("\n"))
        # The same real message's footer is 18 px above its card bottom.
        bands = self.input_bands()
        self.click(self.width - 148, bands[-1][1] - 13)
        self.check(self.copied_draft() == expected,
                   "the actual quote button inserts the same editable original-ID quotation")
        self.key("Right")
        self.screenshot("desktop-editable-quoted-reply")
        self.key("ctrl+Return")
        quoted_record = self.wait_record(expected)
        quoted_delivery = self.deliver(quoted_record, expected)
        self.report["quoted_delivery"] = {"record": quoted_record, "message": quoted_delivery}
        final = self.native("ssmcp.exec.list")["jobs"]
        self.check(len(final) == 1 and final[0]["job_id"] == "main"
                   and final[0]["pid"] == baseline[0]["pid"],
                   "quoted @/$/slash text creates no execution and leaves the original shell unchanged")
        self.check(len(self.deliveries) == 2, "the two explicit desktop submissions have exactly two deliveries")
        self.check(self.external_invocations("session-service") == [], "the owned desktop fixture starts no external model")
        self.screenshot("desktop-after-verified-send")
        self.desktop.wait(timeout=65)
        self.check(self.desktop.returncode == 0, "the owned real desktop exits normally at its bounded deadline")
        frames = [json.loads(line) for line in (self.artifact_dir / "frames.jsonl").read_text().splitlines()]
        self.check(any(row["width"] == self.width and row["height"] == self.height
                       and row["pid"] == self.desktop.pid for row in frames),
                   "the actual resized window has a matching post-present receipt")

    def finish_inputs(self):
        errors = []
        if self.desktop is not None and self.desktop.poll() is None:
            self.stop(self.desktop)
        if self.xdisplay is not None:
            self.xdisplay.close()
        if self.xvfb is not None:
            self.xvfb.close()
        try:
            self.stop_manager()
        except Exception as error:
            errors.append(str(error))
        try:
            self.cleanup()
        except Exception as error:
            errors.append(str(error))
        self.report["cleanup"] = {
            "owned_processes": [{"pid": process.pid, "exit_code": process.poll(),
                                "reaped": process.returncode is not None} for process in self.processes],
            "xvfb_exit_code": self.xvfb.process.poll() if self.xvfb else None,
            "fixture_exit_code": self.account_gateway.exit_code, "errors": errors}
        if errors or any(process.poll() is None for process in self.processes):
            self.report["status"] = "failed"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--fixture-binary", type=Path, required=True)
    parser.add_argument("--desktop-binary", type=Path, required=True)
    parser.add_argument("--desktop-sha256", required=True)
    parser.add_argument("--desktop-build-commit", required=True)
    parser.add_argument("--native-build-commit", required=True)
    parser.add_argument("--report", type=Path, required=True)
    options = parser.parse_args()
    options.bin_dir = options.bin_dir.resolve(strict=True)
    options.fixture_binary = options.fixture_binary.resolve(strict=True)
    options.desktop_binary = options.desktop_binary.resolve(strict=True)
    options.report = options.report.resolve()
    options.report.parent.mkdir(mode=0o700, parents=True, exist_ok=False)
    report = {"status": "failed", "scope": "Real Linux desktop, private X11, native account and session, exact HTTP delivery",
              "models_started": False, "downloads": False, "live_user_session": False,
              "desktop_build_commit": options.desktop_build_commit, "native_build_commit": options.native_build_commit,
              "binaries": {name: identity(path) for name, path in
                 [("desktop", options.desktop_binary), ("client", options.bin_dir / "ssmcp"),
                  ("router_fixture", options.fixture_binary)]}}
    began = time.monotonic()
    try:
        if report["binaries"]["desktop"]["sha256"] != options.desktop_sha256:
            raise ValueError("desktop binary does not match the supplied immutable build receipt")
        with tempfile.TemporaryDirectory(prefix="ssmcp-desktop-input-") as directory:
            harness = DesktopInputs(Path(directory), options, report)
            try:
                harness.run_inputs()
                report["status"] = "passed"
            except Exception as error:
                report["error"] = f"{type(error).__name__}: {error}"
                report["traceback"] = traceback.format_exc()
                try:
                    if harness.xdisplay is not None:
                        harness.screenshot("desktop-failure")
                except Exception:
                    pass
                harness.diagnostics()
            finally:
                harness.finish_inputs()
                report["checks"] = harness.checks
    except Exception as error:
        report["error"] = f"{type(error).__name__}: {error}"
        report["traceback"] = traceback.format_exc()
    report["seconds"] = round(time.monotonic() - began, 3)
    options.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "report": str(options.report),
                      "checks": len(report.get("checks", [])), "error": report.get("error")}, ensure_ascii=False))
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    raise SystemExit(main())
