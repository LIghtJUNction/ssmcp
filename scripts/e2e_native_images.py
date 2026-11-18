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
            "if not stopped: time.sleep(10)\n",
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

    def tap(self, name):
        key = self.display.keysym_to_keycode(self.XK.string_to_keysym(name))
        if not key:
            raise AssertionError("no keysym in owned Xvfb: " + name)
        self.xtest.fake_input(self.display, self.X.KeyPress, key)
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
        for process in (self.terminal, self.xvfb):
            if process is not None and process.poll() is None:
                with contextlib.suppress(ProcessLookupError):
                    process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=3)
        if self.display is not None:
            self.display.close()
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
    try:
        credentials = native.connection()
        harness.client_id = credentials["client_id"]
        harness.sensitive.append(credentials["login_code"])
        harness.oauth_login(credentials)
        native.wait(lambda: harness.ui("/display/rows") == 32, "live candidate UI")
        harness.check(harness.ui("/display/conversation/draft/text") == "",
                      "real graphics and cell-size replies do not enter the composer")

        def read_image():
            result = harness.response(
                "const result=await call(%s,'ssmcp.fs.read_image',{path:'native-grid.png'});"
                "image(result); return result.structuredContent;" % json.dumps(harness.client_id)
            )
            blocks = [block for block in result.get("content", []) if block.get("type") == "image"]
            expected = (harness.workspace / "native-grid.png").read_bytes()
            harness.check(len(blocks) == 1 and base64.b64decode(blocks[0]["data"], validate=True) == expected,
                          "the original authorized PNG remains an unchanged real MCP image block")

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

        harness.observe("ssmcp.ui.action", {"action": "logs.clear"})
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
        harness.observe("ssmcp.ui.message", {"text": "\n".join(f"- Scroll line {i}" for i in range(48))})
        absent("new text scrolls the earlier image out without leaving an overlay")
        for _ in range(6):
            harness.observe("ssmcp.ui.action", {"action": "logs.page-up"})
            time.sleep(0.06)
            if locate_grid(native.image(), raster):
                break
        visible("scrolling back restores the image at its current event position")
        native.image().save(output / "scrolled-image.png")
        native.resize(24, 80)
        native.wait(lambda: harness.ui("/display/rows") == 24
                    and harness.ui("/display/cols") == 80, "actual ztty PTY resize")
        harness.observe("ssmcp.ui.action", {"action": "logs.clear"})
        read_image()
        x, y = visible("native image is redrawn after an actual terminal resize")
        panes = harness.ui("/display/panes")
        harness.check(12 <= x and x + raster.width <= 12 + panes["logs"]["cols"] * native.cw
                      and 12 + panes["logs"]["row"] * native.ch <= y
                      and y + raster.height <= 12 + panes["input"]["row"] * native.ch,
                      "resized image pixels remain inside Activity, above the composer")
        native.image().save(output / "resized-image.png")
        harness.observe("ssmcp.ui.action", {"action": "logs.clear"})
        absent("clearing Activity removes native image pixels")
        native.image().save(output / "cleared-image.png")
        read_image()
        visible("an image can be displayed again after cache cleanup")
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
    decoder = args.bin_dir / "examples/vt_snapshot"
    binaries = {"ssmcp": args.bin_dir / "ssmcp", "ssmcpd": args.bin_dir / "ssmcpd",
                "vt_snapshot": decoder, "ztty": args.ztty}
    for path in binaries.values():
        if not path.is_file():
            parser.error("missing already-built binary: " + str(path))
    args.out.mkdir(parents=True, exist_ok=True)
    report = {"status": "failed", "binaries": {name: sha256(path) for name, path in binaries.items()}}
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
            for name in ("ztty.stderr", "xvfb.stderr"):
                source = temporary / name
                if source.exists():
                    shutil.copyfile(source, args.out / name)
            report.update(checks=harness.checks, metrics=harness.metrics,
                          duration_seconds=round(time.monotonic() - started, 3))
            (args.out / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]),
                      "report": str(args.out / "report.json")}), flush=True)
    if report["status"] != "passed":
        raise SystemExit(1)


if __name__ == "__main__":
    main()
