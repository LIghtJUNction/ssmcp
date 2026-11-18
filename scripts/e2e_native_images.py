#!/usr/bin/env python3
"""Verify native image pixels in an owned Xvfb/ztty and a real ssmcp PTY.

Requires already-built candidate ssmcp/ssmcpd/vt_snapshot, ztty with static Kitty
graphics, Xvfb, Pillow and python-Xlib. Reuses only the isolated SSH/OAuth fixture;
no model, shared terminal, host display, or production endpoint is used.
"""
import argparse
import base64
import contextlib
import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time

from e2e_dashboard import sha256
from e2e_input_history import InputHistory


def binary_identity(path):
    result = subprocess.run([str(path), "--version"], capture_output=True,
                            text=True, timeout=5)
    if result.returncode:
        raise RuntimeError("cannot identify terminal version: " + str(path))
    return {"path": str(path), "version": (result.stdout or result.stderr).strip(),
            "sha256": sha256(path)}


def export_logs(harness, directory, output):
    for name in ("ztty.stderr", "xvfb.stderr"):
        path = directory / name
        if path.exists():
            text = path.read_text(errors="replace")
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    text = text.replace(secret, "[redacted]")
            (output / name).write_text(text)


class Window:
    def __init__(self, harness, binary, directory):
        self.process_info = directory / "owned-processes.json"
        self.display = self.window = self.terminal = self.xvfb = None
        self.logs = []
        try:
            self._start(harness, binary, directory)
        except BaseException:
            self.close()
            raise

    def _start(self, harness, binary, directory):
        from PIL import Image
        from Xlib import X, XK, display
        from Xlib.ext import xtest

        self.harness, self.directory = harness, directory
        self.Image, self.X, self.XK, self.xtest = Image, X, XK, xtest
        self.display = self.window = self.terminal = self.xvfb = None
        self.logs = []
        self.credentials = directory / "connection.jsonl"
        self.process_info = directory / "owned-processes.json"
        self.exit_info = directory / "client-exit.json"
        read_fd, write_fd = os.pipe()
        log = open(directory / "xvfb.stderr", "wb")
        self.logs.append(log)
        try:
            self.xvfb = subprocess.Popen(
                ["Xvfb", "-displayfd", str(write_fd), "-screen", "0", "1600x1000x24",
                 "-nolisten", "tcp"], pass_fds=(write_fd,), stdout=subprocess.DEVNULL,
                stderr=log, start_new_session=True,
            )
        finally:
            os.close(write_fd)
        try:
            if not select.select([read_fd], [], [], 10)[0]:
                raise RuntimeError("owned Xvfb did not become ready")
            number = os.read(read_fd, 64).strip().decode("ascii")
            if not number.isdecimal():
                raise RuntimeError("Xvfb did not return an isolated display number")
        finally:
            os.close(read_fd)
        self.display_name = ":" + number
        self.display = display.Display(self.display_name)
        environment = harness.client_environment("native-image-window")
        git = shutil.which("git")
        if not git:
            raise RuntimeError("Git is required by the existing fixture")
        (Path(environment["PATH"]) / "git").symlink_to(Path(git).absolute())
        environment.update(harness.safe_git_env)
        environment.update({
            "DISPLAY": self.display_name, "LC_ALL": "C.UTF-8", "XMODIFIERS": "@im=none",
            "ZTTY_SOCKET": "@ssmcp-image-test-" + str(os.getpid()),
            "SSMCP_UI_CONFIG": str(harness.config("native-image")),
        })
        for name in ("NO_COLOR", "TMUX", "STY"):
            environment.pop(name, None)
        config = directory / "ztty.conf"
        config.write_text("window-padding = 12\nbackground = #ffffff\n"
                          "background-opacity = 100\ncursor-blink-interval = 150\n")
        command = [
            str(harness.binary_dir / "ssmcp"), "--ssh", "-F",
            str(harness.directory / "ssh_config"), "--name", "native-image-fixture",
            "--shell", "/bin/sh", f"{harness.user}@ssmcp-e2e",
        ]
        launcher = directory / "run-client.py"
        launcher.write_text(
            "import json,os,signal,subprocess,time\n"
            "child=None\nstopped=False\n"
            "def stop(_signal,_frame):\n"
            "    global stopped\n    stopped=True\n"
            "    if child is not None and child.poll() is None: child.terminate()\n"
            "signal.signal(signal.SIGTERM,stop)\nsignal.signal(signal.SIGHUP,stop)\n"
            "signal.signal(signal.SIGINT,lambda *_:None)\n"
            f"with open({str(self.credentials)!r},'wb',buffering=0) as output:\n"
            f"    child=subprocess.Popen({command!r},stdout=output)\n"
            "    started=open(f'/proc/{child.pid}/stat').read().rsplit(')',1)[1].split()[19]\n"
            f"    open({str(self.process_info)!r},'w').write(json.dumps({{'client':child.pid,'wrapper':os.getpid(),'started':started}}))\n"
            "    status=child.wait()\n"
            f"open({str(self.exit_info)!r},'w').write(json.dumps({{'exit_code':status}}))\n"
            ,
            encoding="utf-8",
        )
        log = open(directory / "ztty.stderr", "wb")
        self.logs.append(log)
        self.terminal = subprocess.Popen(
            [str(binary), "--config", str(config), "--font", "monospace:size=12",
             "--geometry", "120x32", "-e", sys.executable, str(launcher)],
            cwd=harness.workspace, env=environment, stdout=subprocess.DEVNULL, stderr=log,
            start_new_session=True,
        )

        def find_window():
            for window in self.display.screen().root.query_tree().children:
                name = window.get_wm_class()
                if name and any(part.lower() == "ztty" for part in name):
                    self.window = window
                    return True
            return False

        self.wait(find_window, "owned ztty window", 12)
        self.window.configure(x=0, y=0)
        self.window.set_input_focus(X.RevertToParent, X.CurrentTime)
        self.display.sync()
        geometry = self.window.get_geometry()
        self.cw, self.ch = (geometry.width - 24) // 120, (geometry.height - 24) // 32
        if min(self.cw, self.ch) <= 0:
            raise AssertionError("ztty returned invalid cell geometry")

    def wait(self, predicate, label, timeout=10):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            if self.terminal and self.terminal.poll() is not None:
                raise AssertionError("owned ztty exited while waiting for " + label)
            value = predicate()
            if value:
                return value
            time.sleep(0.025)
        raise AssertionError("timed out waiting for " + label)

    def connection(self, previous=None):
        def latest():
            if not self.credentials.exists():
                return None
            for line in reversed(self.credentials.read_bytes().splitlines()):
                try:
                    value = json.loads(line)
                except (ValueError, UnicodeDecodeError):
                    continue
                if value.get("client_id") and value["client_id"] != previous:
                    return value
            return None
        return self.wait(latest, "private connection record", 20)

    def image(self):
        geometry = self.window.get_geometry()
        raw = self.window.get_image(0, 0, geometry.width, geometry.height,
                                    self.X.ZPixmap, 0xffffffff).data
        return self.Image.frombytes("RGB", (geometry.width, geometry.height), raw, "raw", "BGRX")

    def tap(self, name, modifiers=()):
        names = (*modifiers, name)
        keys = [self.display.keysym_to_keycode(self.XK.string_to_keysym(part))
                for part in names]
        if not all(keys):
            raise AssertionError("no keysym in owned Xvfb: " + repr(names))
        pressed = []
        try:
            for key in keys:
                self.xtest.fake_input(self.display, self.X.KeyPress, key)
                pressed.append(key)
        finally:
            for key in reversed(pressed):
                self.xtest.fake_input(self.display, self.X.KeyRelease, key)
            self.display.sync()

    def resize(self, rows, cols):
        self.window.configure(width=cols * self.cw + 24, height=rows * self.ch + 24)
        self.display.sync()

    def close(self):
        if self.process_info.exists() and not self.exit_info.exists():
            with contextlib.suppress(OSError, ValueError, KeyError):
                record = json.loads(self.process_info.read_text())
                pid = record["client"]

                def same_child():
                    try:
                        current = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[19]
                        return current == record["started"]
                    except (OSError, IndexError):
                        return False

                if same_child():
                    os.kill(pid, signal.SIGTERM)
                    until = time.monotonic() + 3
                    while same_child() and not self.exit_info.exists() and time.monotonic() < until:
                        time.sleep(0.025)
                    if not self.exit_info.exists() and same_child():
                        os.kill(pid, signal.SIGKILL)
        if self.display is not None:
            with contextlib.suppress(Exception):
                self.display.close()
            self.display = None
        for process in (self.terminal, self.xvfb):
            if process is not None and process.poll() is None:
                with contextlib.suppress(ProcessLookupError):
                    process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
        for log in self.logs:
            log.close()


def grid_image():
    from PIL import Image
    image = Image.new("RGB", (128, 64))
    pixels = image.load()
    for y in range(image.height):
        for x in range(image.width):
            pixels[x, y] = (247, 31, 73) if (x + y) % 2 == 0 else (13, 217, 167)
    return image


def detail_image(width, height):
    """Synthetic text and single-pixel details; never includes user content."""
    from PIL import Image, ImageDraw, ImageFont
    raster = Image.new("RGB", (width, height), (248, 250, 252))
    draw = ImageDraw.Draw(raster)
    try:
        font = ImageFont.truetype("DejaVuSansMono.ttf", 16)
    except OSError:
        font = ImageFont.load_default()
    for top in range(0, height, 96):
        draw.rectangle((0, top, width - 1, min(top + 94, height - 1)),
                       fill=(248, 250, 252) if top % 192 == 0 else (237, 244, 251))
        draw.text((12, top + 10), f"Native pixels | source row {top:04d}",
                  font=font, fill=(20, 28, 40))
        draw.text((12, top + 35), "Aa Bb 0123456789 / const value = 1;",
                  font=font, fill=(20, 28, 40))
        for x in range(8, width - 8):
            color = (247, 31, 73) if x % 2 == 0 else (13, 217, 167)
            draw.line((x, top + 64, x, min(top + 79, height - 1)), fill=color)
        draw.line((0, top + 95, width - 1, top + 95), fill=(40, 90, 150))
    return raster


def locate_grid(screen, expected):
    pattern = expected.crop((0, 0, expected.width, 1)).tobytes()
    row_bytes = screen.width * 3
    pixels = screen.tobytes()
    for y in range(screen.height - expected.height + 1):
        row = pixels[y * row_bytes:(y + 1) * row_bytes]
        start = 0
        while True:
            found = row.find(pattern, start)
            if found < 0:
                break
            if found % 3 == 0:
                x = found // 3
                if screen.crop((x, y, x + expected.width, y + expected.height)).tobytes() == expected.tobytes():
                    return (x, y)
            start = found + 1
    return None


def run(harness, binary, temporary, output):
    harness.prepare()
    harness.workspace = temporary / "workspace"
    harness.workspace.mkdir()
    subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "-q"],
                   cwd=harness.workspace, env={**os.environ, **harness.safe_git_env,
                                               "HOME": str(harness.home)},
                   check=True, timeout=5, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
    raster = grid_image()
    raster.save(harness.workspace / "native-grid.png")
    raster.save(output / "source-grid.png")
    native = Window(harness, binary, temporary)
    ready = False
    try:
        credentials = native.connection()
        harness.client_id = credentials["client_id"]
        harness.sensitive.append(credentials["login_code"])
        harness.oauth_login(credentials)
        native.wait(lambda: harness.ui("/display/rows") == 32, "live candidate UI")
        ready = True
        harness.check(harness.ui("/display/conversation/draft/text") == "",
                      "real graphics and cell-size replies do not enter the composer")

        def observe(name, arguments=None):
            # UI actions return a full state snapshot, which can legitimately
            # spill as this scenario grows. guard_result preserves isError;
            # keep only that checked acknowledgement in code_mode. Narrow UI
            # projections and X11 pixel assertions below verify each effect.
            response = harness.response(
                "const result=await call(%s,%s,%s);"
                "if(result.isError)throw new Error(JSON.stringify(result));"
                "const data=result.structuredContent;"
                "if(data?.spilled&&!data.artifact?.complete)throw new Error('incomplete UI action result');"
                "return {ok:true};"
                % (json.dumps(harness.client_id), json.dumps(name), json.dumps(arguments or {}))
            )
            if harness.messages(response):
                raise AssertionError("an unbound UI action consumed a queued user requirement")
            if response.get("structuredContent", {}).get("result") != {"ok": True}:
                raise AssertionError("native UI action did not return its checked acknowledgement")

        def read_image(name="native-grid.png"):
            result = harness.response(
                "const result=await call(%s,'ssmcp.fs.read_image',{path:%s});"
                "image(result); return result.structuredContent;"
                % (json.dumps(harness.client_id), json.dumps(name))
            )
            blocks = [block for block in result.get("content", []) if block.get("type") == "image"]
            expected = (harness.workspace / name).read_bytes()
            harness.check(len(blocks) == 1 and base64.b64decode(blocks[0]["data"], validate=True) == expected,
                          name + " remains an unchanged real MCP image block")

        def visible(label):
            point = native.wait(lambda: locate_grid(native.image(), raster), label)
            harness.check(True, label)
            return point

        fixture_colors = (bytes((247, 31, 73)), bytes((13, 217, 167)))

        def has_fixture_pixels():
            pixels = native.image().tobytes()
            for color in fixture_colors:
                start = 0
                while (found := pixels.find(color, start)) >= 0:
                    if found % 3 == 0:
                        return True
                    start = found + 1
            return False

        harness.check(not has_fixture_pixels(), "fixture colors are absent before native image upload")

        def absent(label):
            native.wait(lambda: not has_fixture_pixels(), label)
            # Require several settled frames and reject partial image remnants.
            time.sleep(0.15)
            harness.check(not has_fixture_pixels(), label)

        observe("ssmcp.ui.action", {"action": "logs.clear"})
        read_image()
        point = visible("native ztty pixels preserve the complete single-pixel 128 x 64 grid")
        native.image().save(output / "native-image.png")
        harness.metrics["native_grid_origin"] = point
        harness.metrics["cell_pixels"] = [native.cw, native.ch]
        native.tap("F1")
        native.wait(lambda: harness.ui("/display/help"), "help view")
        absent("help removes the image placement")
        native.image().save(output / "help-without-image.png")
        native.tap("Escape")
        native.wait(lambda: not harness.ui("/display/help"), "Activity view")
        visible("returning to Activity restores the cached native image")
        observe("ssmcp.ui.message", {"text": "\n".join(f"- Scroll line {i}" for i in range(48))})
        absent("new text scrolls the earlier image out without leaving an overlay")
        for _ in range(6):
            observe("ssmcp.ui.action", {"action": "logs.page-up"})
            time.sleep(0.06)
            if locate_grid(native.image(), raster):
                break
        visible("scrolling back restores the image at its current event position")
        native.image().save(output / "scrolled-image.png")
        native.resize(24, 80)
        native.wait(lambda: harness.ui("/display/rows") == 24
                    and harness.ui("/display/cols") == 80, "actual ztty PTY resize")
        observe("ssmcp.ui.action", {"action": "logs.clear"})
        read_image()
        x, y = visible("native image is redrawn after an actual terminal resize")
        panes = harness.ui("/display/panes")
        harness.check(12 <= x and x + raster.width <= 12 + panes["logs"]["cols"] * native.cw
                      and 12 + panes["logs"]["row"] * native.ch <= y
                      and y + raster.height <= 12 + panes["input"]["row"] * native.ch,
                      "resized image pixels remain inside Activity, above the composer")
        native.image().save(output / "resized-image.png")
        observe("ssmcp.ui.action", {"action": "logs.clear"})
        absent("clearing Activity removes native image pixels")
        native.image().save(output / "cleared-image.png")
        read_image()
        visible("an image can be displayed again after cache cleanup")

        # A long image must retain its source pixels and open at its heading.
        # Matching X11 readback against complete source crops catches both the
        # old 1024-pixel thumbnail and the former 18-row height squeeze.
        native.resize(32, 120)
        native.wait(lambda: harness.ui("/display/rows") == 32
                    and harness.ui("/display/cols") == 120, "wide image-test viewport")
        tall = detail_image(600, 2400)
        tall.save(harness.workspace / "native-tall.png")
        tall.save(output / "source-tall.png")
        observe("ssmcp.ui.action", {"action": "logs.clear"})
        read_image("native-tall.png")

        def crop_visible(source, y, label):
            crop = source.crop((0, y, source.width, min(source.height, y + 128)))
            point = native.wait(lambda: locate_grid(native.image(), crop), label)
            harness.check(True, label)
            return point

        top = crop_visible(tall, 0, "2400-pixel-tall image opens with pixel-exact text and 1px details at the top")
        initial = harness.ui("/display/events")
        harness.check(not initial["following"] and initial["rows_below"] > 0,
                      "a new tall image pauses at its heading with more image rows below")
        native.image().save(output / "tall-image-top.png")
        observe("ssmcp.ui.message", {"text": "Later activity preserves the image reading position."})
        after_message = harness.ui("/display/events")
        harness.check(after_message["scroll"] == initial["scroll"]
                      and not after_message["following"],
                      "later activity does not move a reader inspecting the tall image")
        harness.check(crop_visible(tall, 0, "the top image crop survives later activity") == top,
                      "new activity keeps the native image at the same screen position")
        observe("ssmcp.ui.action", {"action": "logs.page-down"})
        native.wait(lambda: harness.ui("/display/events/scroll") > initial["scroll"],
                    "long-image scroll position")
        scrolled = harness.ui("/display/events")
        source_y = (scrolled["scroll"] - initial["scroll"] - 2) * native.ch
        harness.check(0 < source_y < tall.height - 128,
                      "scrolling selects an interior source interval")
        middle = crop_visible(tall, source_y, "scrolling preserves pixel-exact interior text and 1px details")
        harness.check(middle[0] == top[0], "scrolling a tall image preserves its horizontal scale")
        native.image().save(output / "tall-image-scrolled.png")
        native.tap("End", ("Control_L",))
        native.wait(lambda: harness.ui("/display/events/following")
                    and harness.ui("/display/events/rows_below") == 0, "physical Ctrl+End resumes follow")
        crop_visible(tall, tall.height - 128, "Ctrl+End reveals the pixel-exact bottom of the long image")
        native.image().save(output / "tall-image-bottom.png")
        harness.metrics["tall_image"] = {
            "source_pixels": list(tall.size), "top_origin": top,
            "interior_origin": middle, "interior_source_y": source_y,
            "initial_following": initial["following"], "ctrl_end_following": True,
        }

        # Two source pixels per displayed pixel make width fitting verifiable
        # without assuming a particular terminal interpolation algorithm.
        panes = harness.ui("/display/panes")
        fitted_width = 12 + panes["logs"]["cols"] * native.cw - top[0]
        # This positive geometry test stays within ztty's default 16 MiB
        # decoded-image budget. Test a terminal rejection separately below.
        fitted = detail_image(fitted_width, 864)
        wide = fitted.resize((fitted.width * 2, fitted.height * 2), native.Image.Resampling.NEAREST)
        harness.check(wide.width * wide.height * 4 <= 16 * 1024 * 1024,
                      "the width-fitting fixture stays within ztty's decoded-image budget")
        wide.save(harness.workspace / "native-width-fit.png")
        wide.save(output / "source-width-fit.png")
        observe("ssmcp.ui.action", {"action": "logs.clear"})
        read_image("native-width-fit.png")
        fitted_top = crop_visible(fitted, 0, "wide source fits Activity width and retains visible text and 1px details")
        fitted_initial = harness.ui("/display/events")
        harness.check(not fitted_initial["following"] and fitted_initial["rows_below"] > 0,
                      "width fitting leaves the long image scrollable")
        harness.check(fitted_top[0] + fitted.width == 12 + panes["logs"]["cols"] * native.cw,
                      "the width-fitted image reaches the Activity edge without overflow")
        native.image().save(output / "width-fit-image-top.png")
        observe("ssmcp.ui.action", {"action": "logs.page-down"})
        native.wait(lambda: harness.ui("/display/events/scroll") > fitted_initial["scroll"],
                    "width-fitted image scroll position")
        fitted_scroll = harness.ui("/display/events/scroll")
        fitted_y = (fitted_scroll - fitted_initial["scroll"] - 2) * native.ch
        fitted_middle = crop_visible(fitted, fitted_y, "width-fitted source cropping preserves text and single display pixels")
        harness.check(fitted_middle[0] == fitted_top[0], "width-fitted scrolling keeps a constant scale")
        native.image().save(output / "width-fit-image-scrolled.png")
        harness.metrics["width_fitted_image"] = {
            "source_pixels": list(wide.size), "display_pixels": list(fitted.size),
            "top_origin": fitted_top, "interior_origin": fitted_middle,
            "interior_display_y": fitted_y,
        }

        # A compressed PNG can fit ssmcp's transfer/cache budgets yet exceed a
        # terminal's decoded-image budget. That rejection must affect one image,
        # and must not disable later native uploads in the same client session.
        oversized_display = detail_image(fitted_width, 1200)
        oversized = oversized_display.resize((fitted_width * 2, 2400), native.Image.Resampling.NEAREST)
        oversized.save(harness.workspace / "native-over-terminal-limit.png")
        oversized.save(output / "source-over-terminal-limit.png")
        harness.check(oversized.width * oversized.height * 4 > 16 * 1024 * 1024,
                      "the rejection fixture exceeds ztty's decoded-image budget")
        observe("ssmcp.ui.action", {"action": "logs.clear"})
        read_image("native-over-terminal-limit.png")

        def rejected_image_settled():
            events = harness.ui("/display/events")
            image_present = any(event["summary"].startswith(f"[IMAGE] {oversized.width} × {oversized.height}")
                                for event in events["events"])
            return image_present and events["image_cache_bytes"] == 0

        native.wait(rejected_image_settled, "terminal rejection releases only the rejected native payload")
        harness.check(True, "terminal rejection releases the rejected payload while retaining its image event")
        harness.check(locate_grid(native.image(), oversized_display.crop((0, 0, fitted_width, 128))) is None,
                      "the rejected image does not leave a native width-fitted placement")
        native.image().save(output / "rejected-image-fallback.png")
        observe("ssmcp.ui.action", {"action": "logs.clear"})
        read_image()
        recovered = visible("a later small image remains pixel-exact native after a terminal rejection")
        native.image().save(output / "native-after-rejection.png")
        harness.metrics["terminal_rejection"] = {
            "source_pixels": list(oversized.size),
            "source_rgba_bytes": oversized.width * oversized.height * 4,
            "ztty_default_decoded_limit_bytes": 16 * 1024 * 1024,
            "rejected_native_cache_bytes": 0,
            "subsequent_small_native_origin": recovered,
            "same_client_session": True,
        }
        previous = harness.client_id
        harness.call("ssmcp.client.reload", {})
        credentials = native.connection(previous)
        harness.client_id = credentials["client_id"]
        harness.sensitive.append(credentials["login_code"])
        harness.oauth_login(credentials)
        native.wait(lambda: harness.ui("/display/active"), "reloaded candidate UI", 20)
        absent("actual client reload leaves no old image placement")
        native.image().save(output / "reloaded-without-image.png")
        harness.check(harness.ui("/display/conversation/draft/text") == "",
                      "the new session's capability probe also leaves the draft empty")
        return {"transport": "candidate ssmcp directly in an owned ztty PTY",
                "display": "private Xvfb", "pixels": "X11 window readback"}
    except BaseException:
        if ready:
            with contextlib.suppress(Exception):
                native.image().save(output / "failure.png")
        raise
    finally:
        native.close()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--ztty", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    def interrupted(_signal, _frame):
        raise TimeoutError("native image fixture interrupted; cleaning up owned processes")
    signal.signal(signal.SIGTERM, interrupted)
    args.bin_dir, args.ztty, args.out = args.bin_dir.resolve(), args.ztty.resolve(), args.out.resolve()
    decoder = args.bin_dir / "vt_snapshot"
    if not decoder.is_file():
        decoder = args.bin_dir / "examples/vt_snapshot"
    binaries = {"ssmcp": args.bin_dir / "ssmcp", "ssmcpd": args.bin_dir / "ssmcpd",
                "vt_snapshot": decoder, "ztty": args.ztty}
    for path in binaries.values():
        if not path.is_file():
            parser.error("missing already-built binary: " + str(path))
    args.out.mkdir(parents=True, exist_ok=True)
    report = {"status": "failed", "binaries": {name: sha256(path) for name, path in binaries.items()}}
    report["terminal"] = binary_identity(args.ztty)
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-native-images-") as directory:
        temporary = Path(directory)
        harness = InputHistory(temporary, args.bin_dir, decoder, args.out)
        try:
            report.update(run(harness, args.ztty, temporary, args.out))
            report["status"] = "passed"
        except Exception as error:
            detail = f"{type(error).__name__}: {error}"
            for secret in [harness.secret, *harness.sensitive]:
                if secret:
                    detail = detail.replace(secret, "[redacted]")
            report["error"] = detail
            print(detail, flush=True)
        finally:
            harness.close_frontends()
            harness.cleanup()
            export_logs(harness, temporary, args.out)
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            (args.out / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "report": str(args.out / "report.json")}), flush=True)
    if report["status"] != "passed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
