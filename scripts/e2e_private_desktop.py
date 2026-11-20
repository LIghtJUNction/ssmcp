#!/usr/bin/env python3
"""Verify owned desktop input and cleanup through real SSH/OAuth/HTTP MCP."""
from __future__ import annotations
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import time
from e2e import wait_for
from e2e_computer_use import ComputerUse
from private_xvfb import PrivateXvfb

class PrivateDesktop(ComputerUse):
    def __init__(self, directory, binary_dir, evidence_dir):
        super().__init__(directory, binary_dir, evidence_dir, False, "")
        self.host_env = None
        self.host_server = None

    def client_environment(self, label, **kwargs):
        env = super().client_environment(label, **kwargs)
        for variable in ("WAYLAND_DISPLAY", "WAYLAND_SOCKET", "HYPRLAND_INSTANCE_SIGNATURE"):
            env.pop(variable, None)
        bindir = Path(env["PATH"])
        for name in ("Xvfb", "dbus-run-session", "dbus-daemon", "wish"):
            source = shutil.which(name)
            if source and not (bindir / name).exists():
                (bindir / name).symlink_to(source)
        return env

    def host_state(self):
        def xdo(*args):
            return subprocess.check_output([shutil.which("xdotool"), *args],
                env=self.host_env, text=True, timeout=5).strip()
        return {"pointer":xdo("getmouselocation", "--shell"), "focus":xdo("getwindowfocus")}

    def run(self):
        self.prepare()
        self.host_server = PrivateXvfb(self.directory / "private-host-display")
        self.host_server.start()
        number = str(self.host_server.number)
        self.check(30000 <= int(number) < 50000, "test host display is privately allocated in the high range")
        self.host_env = self.host_server.environment
        self.spawn("host-sentinel", [shutil.which("xmessage"), "-title",
            "ssmcp host sentinel", "-geometry", "200x120+0+0", "host input sentinel"], env=self.host_env)
        sentinel = wait_for(lambda: subprocess.run([shutil.which("xdotool"),
            "search","--onlyvisible","--name","^ssmcp host sentinel$"],
            env=self.host_env,capture_output=True,text=True,timeout=5).stdout.strip().splitlines(),
            description="test host sentinel")[0]
        subprocess.run([shutil.which("xdotool"),"windowfocus","--sync",sentinel],env=self.host_env,check=True)
        subprocess.run([shutil.which("xdotool"),"mousemove","17","19"],env=self.host_env,check=True)
        baseline = self.host_state()
        workspace = self.directory/"workspace"
        workspace.mkdir()
        self.client, credentials = self.start_client(label="private-desktop",
            name="private-desktop-fixture",cwd=workspace,
            extra_env={key: self.host_env[key] for key in ("DISPLAY", "XAUTHORITY", "XDG_SESSION_TYPE")})
        self.oauth_login(credentials)
        self.client_id = credentials["client_id"]
        self.check(self.computer("desktop_status")["running"] is False,
            "capability inspection leaves the private desktop stopped")
        self.check(self.rejected("ssmcp.computer.click",{"x":20,"y":20}),
            "input without an explicit surface cannot use the host desktop")
        started = self.computer("desktop_start",width=640,height=480)
        self.check(started["display"] != ":"+number and started["moves_host_pointer"] is False,
            "owned desktop uses a different display from the test host")
        repeated = self.computer("desktop_start",width=640,height=480)
        self.check(repeated["display"] == started["display"], "desktop_start reuses the owned display")
        observed = workspace/"typed.txt"
        clicked = workspace/"clicked.txt"
        runtime_path = workspace/"runtime.txt"
        pid_path = workspace/"app.pid"
        fixture = workspace/"fixture.tcl"
        fixture.write_text(
            "wm title . {ssmcp isolated fixture}\nwm geometry . 400x180+0+0\n"
            "entry .input\nplace .input -x 10 -y 10 -width 380 -height 30\n"
            "focus -force .input\n"
            f"bind .input <KeyRelease> {{set f [open {{{observed}}} w]; puts -nonewline $f [.input get]; close $f}}\n"
            f"button .run -text Check -command {{set f [open {{{clicked}}} w]; puts -nonewline $f clicked; close $f}}\n"
            "place .run -x 10 -y 60 -width 100 -height 40\n"
            f"set f [open {{{runtime_path}}} w]; puts -nonewline $f $env(XDG_RUNTIME_DIR); close $f\n"
            f"set f [open {{{pid_path}}} w]; puts -nonewline $f [pid]; close $f\n")
        self.computer("app_launch",command=["wish",str(fixture)])
        def visible():
            return next((window for window in self.computer("windows")["windows"]
                if window["title"] == "ssmcp isolated fixture"),None)
        window = wait_for(visible,timeout=10,description="owned application window")
        self.computer("focus",window_id=window["window_id"])
        self.check(self.rejected("ssmcp.computer.key",{"key":"DefinitelyNotAKey"}),
            "unsupported native keys return an ordinary input error")
        typed = self.computer("type",text="isolated input")
        self.check(typed["engine"]=="zuse" and typed["backend"]=="x11",
            "Computer dispatch reaches the linked zuse library after a rejected key")
        wait_for(lambda: observed.exists() and observed.read_text()=="isolated input",
            timeout=5,description="private native typing")
        self.check(True,"native typing reaches the private application")
        self.computer("click",x=40,y=80)
        wait_for(lambda: clicked.exists() and clicked.read_text()=="clicked",
            timeout=5,description="private native clicking")
        self.check(True,"native click reaches the private button")
        image = self.screenshot("ssmcp.computer.screenshot",{"max_width":640},
            "private-desktop.jpg")
        self.check(image["desktop"]=="isolated" and image["width"]==640 and image["engine"]=="zuse",
            "private screenshot reports its own surface and coordinates")
        self.check(self.host_state()==baseline,"native input and screenshots preserve the test host cursor and focus")
        runtime = Path(runtime_path.read_text())
        app_pid = int(pid_path.read_text())
        display_socket = Path("/tmp/.X11-unix")/("X"+started["display"][1:])
        self.computer("desktop_stop")
        def cleanup_state():
            return {"state_exists": runtime.parent.exists(),
                "application_alive": Path(f"/proc/{app_pid}").exists(),
                "display_socket_exists": display_socket.exists()}
        try:
            wait_for(lambda: not any(cleanup_state().values()), timeout=10,
                description="owned process and state cleanup")
        except AssertionError as error:
            raise AssertionError(f"{error}: {cleanup_state()}") from error
        self.check(True,"desktop_stop reaps applications and X server before removing owned state")
        self.check(self.rejected("ssmcp.computer.move",{"x":10,"y":10}),
            "stopping the private display never falls back to host input")
        self.check(self.host_state()==baseline,"host pointer and focus remain unchanged after shutdown")
        self.metrics.update({"input_before":baseline,"input_after":self.host_state(),
            "display_scope":"two private displays; no input sent to the operator's desktop"})

    def cleanup(self):
        try:
            super().cleanup()
        finally:
            if self.host_server is not None:
                self.host_server.close()

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir",type=Path,default=Path("target/debug"))
    parser.add_argument("--evidence-dir",type=Path,default=Path("target/private-desktop-evidence"))
    parser.add_argument("--report",type=Path)
    args=parser.parse_args()
    for name in ("Xvfb","xdotool","xmessage","magick","xprop","dbus-run-session","dbus-daemon","wish"):
        if not shutil.which(name): parser.error(f"missing test helper: {name}")
    started=time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-private-desktop-") as temporary:
        harness=PrivateDesktop(Path(temporary),args.bin_dir,args.evidence_dir)
        try: harness.run()
        except Exception:
            harness.diagnostics()
            raise
        finally: harness.cleanup()
        report={"status":"passed","checks":harness.checks,"metrics":harness.metrics,
            "duration_seconds":round(time.monotonic()-started,3)}
        if args.report:
            args.report.parent.mkdir(parents=True,exist_ok=True)
            args.report.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n")
        print(f"Passed {len(harness.checks)} private-desktop checks")

if __name__=="__main__":
    main()
