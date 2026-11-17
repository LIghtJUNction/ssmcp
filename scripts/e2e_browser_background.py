#!/usr/bin/env python3
"""Verify real browser background input in a private Xvfb display.

This never sends desktop input to the user's display. Chromium, its profile,
Xvfb, and the focus-sentinel window are owned by this test and cleaned up.
"""
from __future__ import annotations
import argparse
import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import tempfile
import time

from e2e import wait_for, free_port
from e2e_computer_use import ComputerUse, json_get


class BackgroundBrowser(ComputerUse):
    def __init__(self, directory, binary_dir, evidence_dir, chromium):
        super().__init__(directory, binary_dir, evidence_dir, False, chromium)
        self.display_env = None
        self.xdotool = shutil.which("xdotool")

    def xdo(self, *arguments):
        result = subprocess.run([self.xdotool, *map(str, arguments)],
                                env=self.display_env, capture_output=True,
                                text=True, timeout=5)
        if result.returncode:
            raise AssertionError("isolated X11 observation failed: " + result.stderr)
        return result.stdout.strip()

    def input_state(self):
        pointer = dict(line.split("=", 1)
                       for line in self.xdo("getmouselocation", "--shell").splitlines()
                       if "=" in line)
        return {"focus": self.xdo("getwindowfocus"),
                "x": int(pointer["X"]), "y": int(pointer["Y"])}

    def value_for(self, target, expression):
        return self.browser("evaluate", target_id=target, expression=expression)["value"]

    def run(self):
        self.prepare()
        xvfb = self.spawn("private-display", [shutil.which("Xvfb"), "-displayfd", "1",
                          "-screen", "0", "1280x900x24", "-nolisten", "tcp"],
                          stdout=subprocess.PIPE)
        with selectors.DefaultSelector() as selector:
            selector.register(xvfb.stdout, selectors.EVENT_READ)
            if not selector.select(timeout=10):
                raise AssertionError("private Xvfb did not start")
        display_number = xvfb.stdout.readline().decode().strip()
        self.check(display_number.isdigit(), "Xvfb allocated a private display")
        self.display_env = {**os.environ, "DISPLAY": ":" + display_number,
                            "XDG_SESSION_TYPE": "x11", "WAYLAND_DISPLAY": ""}
        workspace = self.directory / "workspace"
        workspace.mkdir()
        self.client, credentials = self.start_client(
            label="background-browser", name="background-browser-fixture", cwd=workspace,
            extra_env={"DISPLAY": self.display_env["DISPLAY"],
                       "XDG_SESSION_TYPE": "x11", "WAYLAND_DISPLAY": ""})
        self.oauth_login(credentials)
        self.client_id = credentials["client_id"]
        url = self.start_fixture()
        port = free_port()
        self.spawn("isolated-chromium", [self.chromium, "--ozone-platform=x11",
                   "--no-first-run", "--no-default-browser-check", "--disable-background-networking", "--disable-gpu",
                   "--disable-dev-shm-usage", "--remote-debugging-address=127.0.0.1",
                   f"--remote-debugging-port={port}",
                   f"--user-data-dir={self.directory / 'browser-profile'}",
                   "--window-size=1000,800", "about:blank"], env=self.display_env)
        endpoint = f"http://127.0.0.1:{port}"
        wait_for(lambda: json_get(endpoint + "/json/version"), timeout=20,
                 description="private Chromium debugger")
        self.browser("connect", endpoint=endpoint)
        original = self.browser("tabs")["tabs"]
        self.check(len(original) == 1, "private Chromium has one original foreground tab")
        foreground = original[0]["target_id"]
        self.check(self.value_for(foreground, "document.visibilityState") == "visible",
                   "original tab starts visible")
        self.spawn("focus-sentinel", [shutil.which("xmessage"), "-title",
                   "ssmcp focus sentinel", "-geometry", "260x120+0+0", "User input stays here"],
                   env=self.display_env)
        sentinel = wait_for(
            lambda: subprocess.run([self.xdotool, "search", "--onlyvisible", "--name", "^ssmcp focus sentinel$"], env=self.display_env, capture_output=True, text=True, timeout=5).stdout.strip().splitlines(),
            description="private focus sentinel")[0]
        self.xdo("windowfocus", "--sync", sentinel)
        self.xdo("mousemove", "17", "19")
        baseline = self.input_state()
        opened = self.browser("open", url=url)
        self.target = opened["target_id"]
        self.check(opened["background"] is True, "browser.open defaults to a background tab")
        snapshot = self.browser("snapshot", target_id=self.target)
        input_ref = next(e["ref"] for e in snapshot["elements"] if e["tag"] == "input")
        button_ref = next(e["ref"] for e in snapshot["elements"] if e["name"] == "Run check")
        self.browser("fill", target_id=self.target, ref=input_ref, text="background input verified")
        self.browser("click", target_id=self.target, ref=button_ref)
        self.check(self.browser("wait_for", target_id=self.target, text="Checks: 1", timeout_ms=3000)["matched"],
                   "background fill/click/wait_for complete in the real page")
        self.browser("scroll", target_id=self.target, delta_y=380)
        wait_for(lambda: self.evaluate("scrollY") > 0, timeout=5, description="background wheel scrolling")
        self.check(True, "background scroll moves the target viewport")
        self.check(self.evaluate("document.querySelector('#message').value") == "background input verified",
                   "background input reaches only the requested target")
        self.check(self.value_for(foreground, "document.visibilityState") == "visible",
                   "original foreground tab remains visible")
        self.check(self.evaluate("document.visibilityState") == "hidden",
                   "controlled tab remains in the background")
        after = self.input_state()
        self.check(after == baseline, "browser actions preserve independent cursor coordinates and keyboard focus")
        self.metrics["input_before"] = baseline
        self.metrics["input_after"] = after
        self.metrics["original_tab_unchanged"] = True
        self.metrics["background_tab_hidden"] = True
        self.metrics["display_scope"] = "private Xvfb; no desktop input sent to user display"
        self.screenshot("ssmcp.browser.screenshot", {"target_id":self.target},
                        "background-browser.jpg")
        self.check(self.input_state() == baseline
                   and self.value_for(foreground, "document.visibilityState") == "visible",
                   "background screenshot preserves cursor, focus and foreground tab")
        self.browser("close", target_id=self.target)
        self.browser("disconnect")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--evidence-dir", type=Path, default=Path("target/browser-background-evidence"))
    parser.add_argument("--report", type=Path)
    parser.add_argument("--chromium")
    args = parser.parse_args()
    for program in ("Xvfb", "xdotool", "xmessage"):
        if not shutil.which(program):
            parser.error(f"install {program} to run the isolated background browser check")
    candidates = [args.chromium, "/usr/lib/chromium/chromium", "/opt/google/chrome/chrome"]
    chromium = next((p for p in candidates if p and Path(p).is_file()), None)
    if not chromium:
        parser.error("install Chromium or pass --chromium")
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-browser-background-") as directory:
        harness = BackgroundBrowser(Path(directory), args.bin_dir, args.evidence_dir, chromium)
        try:
            harness.run()
        except Exception:
            harness.diagnostics()
            raise
        finally:
            harness.cleanup()
        report = {"status":"passed", "checks":harness.checks, "metrics":harness.metrics,
                  "duration_seconds":round(time.monotonic()-started, 3)}
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2)+"\n")
        print(f"Passed {len(harness.checks)} private background-browser checks")

if __name__ == "__main__":
    main()
