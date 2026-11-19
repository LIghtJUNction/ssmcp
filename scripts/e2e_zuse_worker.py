#!/usr/bin/env python3
"""Exercise the linked zuse library on two private X servers, with real input."""
from __future__ import annotations

import argparse
import base64
import ctypes
import hashlib
import json
import os
from pathlib import Path
import selectors
import shutil
import subprocess
import tempfile
import time


def wait_for(predicate, description, timeout=8):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        result = predicate()
        if result:
            return result
        time.sleep(0.01)
    raise AssertionError("timed out: " + description)


class Harness:
    def __init__(self, root, binary, evidence):
        self.root, self.binary, self.evidence = root, binary.resolve(), evidence
        self.processes, self.checks = [], []
        self.displays = []

    def check(self, condition, description):
        assert condition, description
        self.checks.append(description)

    def spawn(self, *args, **kwargs):
        process = subprocess.Popen(args, **kwargs)
        self.processes.append(process)
        return process

    def display(self, name):
        process = self.spawn(shutil.which("Xvfb"), "-displayfd", "1", "-screen", "0", "640x480x24", "-nolisten", "tcp", stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            assert selector.select(timeout=10), "private display startup"
        number = process.stdout.readline().decode().strip()
        assert number.isdigit()
        env = dict(os.environ, DISPLAY=":"+number, ZUSE_BACKEND="x11", WAYLAND_DISPLAY="", HYPRLAND_INSTANCE_SIGNATURE="")
        self.displays.append(number)
        directory = self.root / name
        directory.mkdir()
        fixture = directory / "fixture.tcl"
        fixture.write_text(
            f"wm title . {{{name}}}\nwm geometry . 400x220+0+0\n"
            "entry .input\nplace .input -x 10 -y 10 -width 380 -height 30\nfocus -force .input\n"
            f"bind .input <KeyRelease> {{set f [open {{{directory/'typed'}}} w]; puts -nonewline $f [.input get]; close $f}}\n"
            f"button .button -text Check -command {{set f [open {{{directory/'clicked'}}} w]; puts -nonewline $f clicked; close $f}}\n"
            "place .button -x 10 -y 60 -width 100 -height 40\n"
            f"bind . <ButtonPress-1> {{set f [open {{{directory/'pressed'}}} w]; puts -nonewline $f down; close $f}}\n"
            f"bind . <ButtonRelease-1> {{set f [open {{{directory/'released'}}} w]; puts -nonewline $f up; close $f}}\n"
        )
        self.spawn(shutil.which("wish"), str(fixture), env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        wait_for(lambda: self.windows(env, name), name + " visible")
        return env, directory

    def start_request(self, env, operation, arguments):
        process = self.spawn(str(self.binary), "--internal-zuse-worker", cwd=self.root, env=env,
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            assert selector.select(timeout=5), "worker readiness"
        assert process.stdout.readline() == b"ssmcp-zuse-worker-v1\n"
        request = {"operation": operation, "arguments": arguments}
        if operation == "capture":
            capture = self.root / ("capture-" + str(process.pid))
            capture.mkdir(mode=0o700)
            request["capture_path"] = str(capture / "capture.png")
        process.stdin.write(json.dumps(request).encode()+b"\n")
        process.stdin.flush()
        return process

    def call(self, env, operation, arguments):
        process = self.start_request(env, operation, arguments)
        with selectors.DefaultSelector() as selector:
            selector.register(process.stdout, selectors.EVENT_READ)
            assert selector.select(timeout=20), "worker response: "+operation
        response = process.stdout.readline()
        process.stdin.close()
        process.stdin = None
        remaining, error = process.communicate(timeout=5)
        self.check(process.returncode == 0, "worker cleanup exit: "+operation)
        assert not remaining, "one operation must produce exactly one response"
        reply = json.loads(response)
        assert reply["cleanup_ok"] and reply["error"] is None, (operation, reply, error.decode(errors="replace"))
        return reply["value"]

    def windows(self, env, title):
        return next((window for window in self.call(env, "windows", {})["result"]["windows"] if window["title"] == title), None)

    @staticmethod
    def x11_state(env):
        lib=ctypes.CDLL("libX11.so.6")
        lib.XOpenDisplay.argtypes=[ctypes.c_char_p]
        lib.XOpenDisplay.restype=ctypes.c_void_p
        lib.XDefaultRootWindow.argtypes=[ctypes.c_void_p]
        lib.XDefaultRootWindow.restype=ctypes.c_ulong
        lib.XGetKeyboardMapping.argtypes=[ctypes.c_void_p,ctypes.c_ubyte,ctypes.c_int,ctypes.POINTER(ctypes.c_int)]
        lib.XGetKeyboardMapping.restype=ctypes.POINTER(ctypes.c_ulong)
        lib.XDisplayKeycodes.argtypes=[ctypes.c_void_p,ctypes.POINTER(ctypes.c_int),ctypes.POINTER(ctypes.c_int)]
        lib.XQueryPointer.argtypes=[ctypes.c_void_p,ctypes.c_ulong,ctypes.POINTER(ctypes.c_ulong),ctypes.POINTER(ctypes.c_ulong),ctypes.POINTER(ctypes.c_int),ctypes.POINTER(ctypes.c_int),ctypes.POINTER(ctypes.c_int),ctypes.POINTER(ctypes.c_int),ctypes.POINTER(ctypes.c_uint)]
        lib.XGetInputFocus.argtypes=[ctypes.c_void_p,ctypes.POINTER(ctypes.c_ulong),ctypes.POINTER(ctypes.c_int)]
        lib.XFree.argtypes=[ctypes.c_void_p]
        lib.XCloseDisplay.argtypes=[ctypes.c_void_p]
        display=lib.XOpenDisplay(env["DISPLAY"].encode())
        assert display
        try:
            root=lib.XDefaultRootWindow(display)
            window,child=ctypes.c_ulong(),ctypes.c_ulong()
            x,y,wx,wy=ctypes.c_int(),ctypes.c_int(),ctypes.c_int(),ctypes.c_int()
            mask=ctypes.c_uint()
            assert lib.XQueryPointer(display,root,ctypes.byref(window),ctypes.byref(child),ctypes.byref(x),ctypes.byref(y),ctypes.byref(wx),ctypes.byref(wy),ctypes.byref(mask))
            focus,revert=ctypes.c_ulong(),ctypes.c_int()
            lib.XGetInputFocus(display,ctypes.byref(focus),ctypes.byref(revert))
            low,high,width=ctypes.c_int(),ctypes.c_int(),ctypes.c_int()
            lib.XDisplayKeycodes(display,ctypes.byref(low),ctypes.byref(high))
            mapping=lib.XGetKeyboardMapping(display,low.value,high.value-low.value+1,ctypes.byref(width))
            assert mapping
            try:
                data=ctypes.string_at(mapping,(high.value-low.value+1)*width.value*ctypes.sizeof(ctypes.c_ulong))
            finally:
                lib.XFree(mapping)
            return {"x":x.value,"y":y.value,"focus":focus.value,"mask":mask.value,"keymap_sha256":hashlib.sha256(data).hexdigest()}
        finally:
            lib.XCloseDisplay(display)

    def run(self):
        host, _=self.display("zuse untouched sentinel")
        private, directory=self.display("zuse native fixture")
        baseline=self.x11_state(host)
        window=self.windows(private,"zuse native fixture")
        self.call(private,"focus",{"window_id":window["window_id"]})
        literal="literal $() ; 中文"
        self.call(private,"type",{"text":literal})
        wait_for(lambda: (directory/"typed").exists() and (directory/"typed").read_text()==literal,"literal Unicode keyboard input")
        self.check(True,"native zuse typing preserves literal shell punctuation and Unicode")
        self.call(private,"click",{"point":{"x":40,"y":80},"button":"left","count":1})
        wait_for(lambda: (directory/"clicked").exists(),"native click")
        self.check(True,"native zuse button input reaches the application")
        before=self.x11_state(private)
        process=self.start_request(private,"drag",{"path":[{"x":150,"y":140},{"x":300,"y":140}],"button":"left","duration_ms":5000})
        wait_for(lambda: self.x11_state(private)["mask"] & 0x100,"native drag button down")
        started=time.monotonic()
        process.stdin.close()
        process.stdin=None
        output,error=process.communicate(timeout=3)
        self.check(process.returncode==0,"cancelled drag exits after cleanup")
        reply=json.loads(output)
        self.check(reply["error"]=="zuse operation cancelled" and reply["cleanup_ok"],"cancelled drag reports cancellation and successful release")
        self.check(time.monotonic()-started<2 and not self.x11_state(private)["mask"] & 0x100,"cancellation wakes a sleeping drag and releases the physical button")
        self.call(private,"click",{"point":{"x":40,"y":20},"button":"left","count":1})
        before=self.x11_state(private)
        process=self.start_request(private,"type",{"text":"界"*300})
        time.sleep(0.1)
        process.stdin.close()
        process.stdin=None
        output,error=process.communicate(timeout=3)
        self.check(process.returncode==0 and json.loads(output)["cleanup_ok"],"cancelled Unicode typing exits with a cleanup receipt")
        after=self.x11_state(private)
        self.check(before["keymap_sha256"]==after["keymap_sha256"],"cancellation restores the native X11 Unicode keyboard mapping")
        self.check(not after["mask"] & 0x1f00,"no pointer button remains held after cancellation")
        capture=self.call(private,"capture",{"desktop":{"x":0,"y":0,"width":640,"height":480},"region":{"x":0,"y":0,"width":640,"height":480},"max_width":640,"quality":70,"cursor_requested":False})
        image=base64.b64decode(capture["data"],validate=True)
        metadata=capture["metadata"]
        self.check(image[:2]==b"\xff\xd8" and len(image)<=400*1024,"zuse native PNG capture is delivered as bounded JPEG")
        self.check(metadata["engine"]=="zuse" and metadata["width"]==640 and metadata["height"]==480,"capture metadata names the actual library and coordinate mapping")
        self.evidence.mkdir(parents=True,exist_ok=True)
        (self.evidence/"native-desktop.jpg").write_bytes(image)
        self.check(self.x11_state(host)==baseline,"two fixed display environments preserve the other desktop pointer, focus and keymap")
        self.check(all(process.poll() is not None for process in self.processes if process.args[-1]=="--internal-zuse-worker"),"all operation workers have exited")

    def cleanup(self):
        for process in reversed(self.processes):
            if process.poll() is None:
                process.terminate()
                try: process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill();process.wait(timeout=5)


def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary",type=Path,required=True)
    parser.add_argument("--evidence-dir",type=Path,required=True)
    parser.add_argument("--report",type=Path,required=True)
    args=parser.parse_args()
    for helper in ("Xvfb","wish"):
        if not shutil.which(helper): parser.error("missing fixture helper: "+helper)
    start=time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-zuse-e2e-") as temporary:
        harness=Harness(Path(temporary),args.binary,args.evidence_dir)
        try: harness.run()
        finally: harness.cleanup()
        report={"schema":"ssmcp.zuse.e2e.v1","status":"passed","checks":harness.checks,
                "duration_seconds":round(time.monotonic()-start,3),
                "environment":"two private Xvfb displays; no operator desktop input",
                "binary_sha256":hashlib.sha256(args.binary.read_bytes()).hexdigest()}
        args.report.parent.mkdir(parents=True,exist_ok=True)
        args.report.write_text(json.dumps(report,ensure_ascii=False,indent=2)+"\n")
        print("Passed",len(harness.checks),"zuse native worker checks")


if __name__=="__main__": main()
