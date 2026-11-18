#!/usr/bin/env python3
"""Exercise native browser/desktop tools through real SSH, OAuth and HTTP MCP.

The default run uses an isolated headless Chromium profile. --desktop additionally
operates only the generated fixture window on the current Linux desktop. No model
or external website is used. Requires system Chromium; desktop requires the
helpers reported by computer.status.
"""
from __future__ import annotations

import argparse
import base64
import contextlib
import functools
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import shutil
import socket
import subprocess
import tempfile
import threading
import time
import urllib.request

from e2e import Harness, wait_for, free_port

TITLE = "ssmcp Computer Use Fixture"
FIXTURE = r"""<!doctype html><html><head><meta charset="utf-8">
<title>ssmcp Computer Use Fixture</title>
<style>
*{box-sizing:border-box}body{margin:0;padding:42px;background:#111318;color:#f4f5f8;font:18px system-ui}
main{max-width:750px;margin:auto}p{color:#b3b9c6;line-height:1.6}h1{font-size:38px;letter-spacing:-1px}
label{display:block;margin:28px 0 10px}input{font:20px system-ui;width:100%;padding:14px;background:#242832;border:1px solid #596171;border-radius:8px;color:white}
button{margin-top:22px;padding:12px 24px;border:0;border-radius:7px;background:#a9c7ff;color:#11213c;font:18px system-ui}
#capture-proof{position:fixed;top:0;left:0;width:32px;height:32px;background:#e53935;pointer-events:none;z-index:10000}
#drag{margin-top:28px;padding:26px;border:1px dashed #626b7e;border-radius:8px;user-select:none}
output{display:block;margin-top:22px;color:#b3efc2;white-space:pre-wrap}footer{padding-top:800px;color:#8da5cd}
</style></head><body><div id="capture-proof" aria-hidden="true"></div><main>
<p>SSMCP / COMPUTER USE</p><h1>Browser and desktop verification</h1>
<p>This temporary page records input received through the native ssmcp tools.</p>
<label for="message">Test message</label><span id="message-name" hidden>Accessible message</span><input id="message" autocomplete="off" aria-label="Fallback message" aria-labelledby="message-name">
<button id="run">Run check</button><output id="result">Waiting for native input.</output>
<div id="drag">Drag across this test area</div><footer id="bottom">Scroll target</footer>
</main><script>
window.probe={clicks:0,trustedClicks:0,moves:0,downs:0,ups:0,wheel:0,keys:[],dragMoves:0};
function show(){document.querySelector('#result').textContent='Input: '+document.querySelector('#message').value+'\nChecks: '+probe.clicks;}
document.querySelector('#run').addEventListener('click',event=>{probe.clicks++;if(event.isTrusted)probe.trustedClicks++;document.querySelector('#capture-proof').style.background='#00a060';show()});
document.querySelector('#message').addEventListener('input',show);
document.addEventListener('keydown',e=>probe.keys.push(e.key));
document.addEventListener('mousemove',e=>{probe.moves++;if(e.buttons)probe.dragMoves++});
document.addEventListener('mousedown',()=>probe.downs++);
document.addEventListener('mouseup',()=>probe.ups++);
document.addEventListener('wheel',e=>probe.wheel+=e.deltaY,{passive:true});
</script></body></html>"""

class QuietHandler(SimpleHTTPRequestHandler):
    def log_message(self, *args):
        pass

def json_get(url):
    try:
        with urllib.request.urlopen(url, timeout=1) as response:
            return json.load(response)
    except (OSError, ValueError):
        return None

class ComputerUse(Harness):
    def __init__(self, directory, binary_dir, evidence_dir, desktop, chromium):
        super().__init__(directory, binary_dir)
        # Wait for the bounded code_mode result (30s), including browser launch
        # (20s) and its cleanup. The generic 10s HTTP limit can expire first.
        self.http_timeout = 35
        self.evidence_dir = evidence_dir
        self.evidence_dir.mkdir(parents=True, exist_ok=True)
        self.desktop = desktop
        self.chromium = chromium
        self.metrics = {}
        self.http_fixture = None
        self.target = None

    def client_environment(self, label, **kwargs):
        environment = super().client_environment(label, **kwargs)
        bindir = Path(environment["PATH"])
        for name in ("hyprctl", "grim", "wtype", "ydotool", "xdotool", "magick", "xprop"):
            program = shutil.which(name)
            if program:
                (bindir / name).symlink_to(Path(program).absolute())
        return environment

    def code(self, source):
        return self.public_call("code_mode", {"code": source})["result"]

    def browser(self, method, **arguments):
        return self.call("ssmcp.browser." + method, arguments)

    def computer(self, method, **arguments):
        if self.desktop and method in {"focus", "move", "click", "drag", "key", "type", "scroll"}:
            arguments.setdefault("desktop", "host")
        return self.call("ssmcp.computer." + method, arguments)

    def evaluate(self, expression):
        return self.browser("evaluate", target_id=self.target, expression=expression)["value"]

    def rejected(self, name, arguments):
        return self.code(
            "try { const r=await call(%s,%s,%s);return {rejected:r.isError===true};}"
            "catch(e){return {rejected:true};}" %
            (json.dumps(self.client_id), json.dumps(name), json.dumps(arguments))
        )["rejected"]

    def screenshot(self, name, arguments, filename, *, modern=True):
        source = "const r=await call(%s,%s,%s); image(r); return r.structuredContent;" % (
            json.dumps(self.client_id), json.dumps(name), json.dumps(arguments))
        status, response, _ = self.rpc("tools/call", {"name": "code_mode", "arguments": {"code": source}}, modern=modern)
        if status != 200 or "error" in response or response["result"].get("isError"):
            raise AssertionError(f"screenshot failed with HTTP {status}: {response.get('error', response.get('result'))}")
        result = response["result"]
        images = [block for block in result["content"] if block.get("type") == "image"]
        self.check(len(images) == 1, f"{name} returns a visible MCP image (modern={modern})")
        block = images[0]
        data = base64.b64decode(block["data"], validate=True)
        self.check(block["mimeType"] == "image/jpeg" and data.startswith(b"\xff\xd8") and len(data) <= 400 * 1024,
                   "image is bounded valid JPEG bytes")
        self.check(block["data"] not in json.dumps(result["structuredContent"])
                   and all(block["data"] not in item.get("text", "") for item in result["content"] if item.get("type") == "text"),
                   "image payload is absent from structured metadata and text")
        (self.evidence_dir / filename).write_bytes(data)
        return result["structuredContent"]["result"]

    def start_fixture(self):
        site = self.directory / "fixture"
        site.mkdir()
        (site / "index.html").write_text(FIXTURE)
        handler = functools.partial(QuietHandler, directory=str(site))
        self.http_fixture = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        threading.Thread(target=self.http_fixture.serve_forever, daemon=True).start()
        return f"http://127.0.0.1:{self.http_fixture.server_port}/index.html"

    def fixture_snapshot(self):
        snapshot = self.browser("snapshot", target_id=self.target)
        return snapshot if snapshot.get("title") == TITLE else None

    def browser_checks(self, url):
        initial = self.browser("status")
        self.check(initial["connected"] is False, "browser starts disconnected without opening a user profile")
        launched = self.browser("launch", executable=self.chromium, headless=not self.desktop, width=1100, height=850)
        self.metrics["browser"] = {key: launched.get(key) for key in ("owned", "browser")}
        endpoint = launched["endpoint"]
        self.check(launched["connected"] and launched["owned"], "native launch owns an isolated system Chromium process")
        self.check(initial["input_scope"] == "browser_target" and initial["moves_system_pointer"] is False,
                   "browser status distinguishes target-local input from desktop-wide input")
        opened = self.browser("open", url=url, **({"background": False} if self.desktop else {}))
        self.check(opened["background"] is (not self.desktop),
                   "new tabs default to background; desktop fixture opts in to foreground")
        self.target = opened["target_id"]
        snapshot = wait_for(self.fixture_snapshot, timeout=15, description="fixture DOM ready")
        tabs = self.browser("tabs")["tabs"]
        self.check(any(tab["target_id"] == self.target and tab["url"] == url for tab in tabs), "native tabs include the opened fixture")
        inputs = [item for item in snapshot["elements"] if item.get("tag", "").lower() == "input"]
        buttons = [item for item in snapshot["elements"] if item.get("name") == "Run check"]
        self.check(bool(inputs) and bool(buttons), "snapshot exposes observed input and button references")
        self.check(inputs[0]["role"] == "textbox" and inputs[0]["name"] == "Accessible message"
                   and buttons[0]["role"] == "button", "snapshot includes implicit roles and aria-labelledby accessible names")
        input_ref, button_ref = inputs[0]["ref"], buttons[0]["ref"]
        self.evaluate("setTimeout(()=>{let p=document.createElement('p');p.id='async-ready';p.textContent='ssmcp async ready';document.body.append(p)},120);true")
        self.check(self.browser("wait_for", target_id=self.target, text="ssmcp async ready", timeout_ms=5000)["matched"],
                   "client-local wait observes asynchronous text without repeated snapshots")
        self.evaluate("setTimeout(()=>document.querySelector('#async-ready').remove(),120);true")
        self.check(self.browser("wait_for", target_id=self.target, text="ssmcp async ready", state="absent", timeout_ms=5000)["matched"],
                   "client-local wait observes disappearing text")
        self.check(self.rejected("ssmcp.browser.wait_for", {"target_id":self.target,"text":"never appears marker","timeout_ms":50}),
                   "client-local wait has a bounded timeout")
        message = "ssmcp 原生浏览器 ✓"
        self.browser("fill", target_id=self.target, ref=input_ref, text=message)
        self.browser("fill", target_id=self.target, ref=input_ref, text="")
        self.check(self.evaluate("document.querySelector(\'#message\').value") == "", "browser fill clears text with an empty string")
        self.browser("fill", target_id=self.target, ref=input_ref, text=message)
        self.browser("key", target_id=self.target, key="A", modifiers=["Control"])
        self.browser("key", target_id=self.target, key="Backspace")
        self.check(self.evaluate("document.querySelector(\'#message\').value") == "", "browser modifier chord and Backspace clear input")
        self.browser("fill", target_id=self.target, ref=input_ref, text=message)
        self.browser("click", target_id=self.target, ref=button_ref)
        self.check(self.evaluate("({text:document.querySelector('#message').value,clicks:probe.clicks})") == {"text": message, "clicks": 1},
                   "native fill and click modify the real Chromium document")
        self.browser("key", target_id=self.target, key="Tab")
        self.check("Tab" in self.evaluate("probe.keys"), "browser key dispatch produces a real keyboard event")
        self.evaluate("scrollTo(0,0);probe.wheel=0;true")
        self.browser("scroll", target_id=self.target, delta_y=360)
        wait_for(lambda: self.evaluate("scrollY > 0 && probe.wheel > 0"), description="browser wheel event and scroll")
        self.check(True, "browser scroll moves the actual viewport")
        self.evaluate("scrollTo(0,0);true")
        self.screenshot("ssmcp.browser.screenshot", {"target_id": self.target, "max_width": 1100}, "browser.jpg")
        self.screenshot("ssmcp.browser.screenshot", {"target_id": self.target, "max_width": 640}, "browser-legacy.jpg", modern=False)
        self.check(self.rejected("ssmcp.browser.connect", {"endpoint": "http://example.com:9222/"}),
                   "browser attach rejects a non-loopback debugger endpoint")
        self.check(self.rejected("ssmcp.browser.click", {"target_id": self.target, "ref": "missing-ref"}),
                   "browser click rejects unobserved element references")
        self.batch_browser_checks()
        self.wait_browser_checks()
        if self.desktop:
            self.desktop_checks()
        self.parallel_browser_checks(url)
        last_snapshot = self.browser("snapshot", target_id=self.target)
        input_ref = next(item["ref"] for item in last_snapshot["elements"] if item.get("tag") == "input")
        self.browser("navigate", target_id=self.target, url="about:blank")
        self.check(self.rejected("ssmcp.browser.fill", {"target_id": self.target, "ref": input_ref, "text": "stale"}),
                   "navigation invalidates references from the prior document")
        self.browser("close", target_id=self.target)
        self.check(not any(tab["target_id"] == self.target for tab in self.browser("tabs")["tabs"]), "close removes only the selected test tab")
        self.browser("disconnect")
        self.check(not self.browser("status")["connected"], "disconnect releases owned browser connection")
        wait_for(lambda: json_get(endpoint + "/json/version") is None, description="owned browser stopped")
        self.check(True, "disconnect stops the browser owned by this connection")

    def batch_browser_checks(self):
        snapshot = self.browser("snapshot", target_id=self.target)
        input_ref = next(item["ref"] for item in snapshot["elements"] if item.get("tag") == "input")
        button_ref = next(item["ref"] for item in snapshot["elements"] if item.get("name") == "Run check")
        before = self.evaluate("probe.clicks")
        message = "ssmcp 批量输入 ✓"
        arguments = {"target_id": self.target, "actions": [
            {"type": "fill", "ref": input_ref, "text": message},
            {"type": "click", "ref": button_ref},
            {"type": "wait", "selector": "#result", "text": f"Checks: {before + 1}", "timeout_ms": 5000},
        ]}
        batch = self.browser("act", **arguments)
        self.check(batch["completed_count"] == 3
                   and [step["type"] for step in batch["completed"]] == ["fill", "click", "wait"]
                   and batch["uses_system_pointer"] is False,
                   "one browser batch completes fill, click and condition wait in order")
        fresh = batch["snapshot"]
        self.check(fresh["snapshot_id"] != snapshot["snapshot_id"]
                   and fresh["target_id"] == self.target
                   and message in fresh["text"]
                   and f"Checks: {before + 1}" in fresh["text"],
                   "batch returns a fresh snapshot containing the observed input and click result")
        self.check(self.evaluate("({text:document.querySelector('#message').value,clicks:probe.clicks})")
                   == {"text": message, "clicks": before + 1},
                   "batch changes the real document exactly once")
        self.check(self.rejected("ssmcp.browser.click", {"target_id": self.target, "ref": button_ref}),
                   "post-batch snapshot invalidates references from the prior snapshot")
        current_button = next(item["ref"] for item in fresh["elements"] if item.get("name") == "Run check")
        failure_args = {"target_id": self.target, "actions": [
            {"type": "click", "ref": current_button},
            {"type": "click", "ref": "missing-ref"},
            {"type": "click", "ref": current_button},
        ]}
        failed = self.code(
            "const r=await browser.act(%s,%s);return {isError:r.isError,report:r.structuredContent};" %
            (json.dumps(self.client_id), json.dumps(failure_args))
        )
        report = failed["report"]
        self.check(failed["isError"] is True and report["completed_count"] == 1
                   and report["failed_index"] == 1 and report["phase"] == "actions"
                   and report["replay_safe"] is False,
                   "failed batch preserves its completed action and failed index")
        self.check(self.evaluate("probe.clicks") == before + 2,
                   "batch stops on the stale reference without running the later click or replaying the first")
        invalid_args = {"target_id": self.target, "actions": [
            {"type": "click", "ref": current_button},
            {"type": "scroll", "delta_y": 100001},
        ]}
        self.check(self.rejected("ssmcp.browser.act", invalid_args)
                   and self.evaluate("probe.clicks") == before + 2,
                   "an invalid later action rejects the entire batch before its first click")
        self.metrics["browser_batch"] = {"actions_completed": batch["completed_count"],
                                          "elapsed_ms": batch["elapsed_ms"],
                                          "partial_failure_completed": report["completed_count"],
                                          "partial_failure_index": report["failed_index"]}

    def wait_browser_checks(self):
        # The temporary page owns this asynchronous DOM change. There is no
        # fixed host sleep and no interaction with another browser profile.
        self.evaluate("""(() => {
            document.querySelector('#async-result')?.remove();
            setTimeout(() => {
                const node=document.createElement('output');
                node.id='async-result';node.textContent='Async condition ready';
                document.querySelector('main').append(node);
            }, 250);
            return true;
        })()""")
        waited = self.browser("wait", target_id=self.target, selector="#async-result",
                              text="Async condition ready", timeout_ms=5000)
        self.check(waited["matched"] is True and waited["state"] == "visible"
                   and self.evaluate("document.querySelector('#async-result').textContent") == "Async condition ready",
                   "condition wait observes asynchronous DOM content in the background tab")
        self.evaluate("document.querySelector('#async-result').hidden=true;true")
        hidden = self.browser("wait", target_id=self.target, selector="#async-result", state="hidden")
        attached = self.browser("wait", target_id=self.target, selector="#async-result", state="attached")
        self.check(hidden["matched"] is True and attached["matched"] is True,
                   "wait distinguishes hidden content from a detached node")
        self.evaluate("document.querySelector('#async-result').remove();true")
        detached = self.browser("wait", target_id=self.target, selector="#async-result", state="detached")
        self.check(detached["matched"] is True
                   and self.rejected("ssmcp.browser.wait", {"target_id": self.target,
                       "selector": "#async-result", "state": "visible", "timeout_ms": 100}),
                   "wait detects removal and returns an error when an unmet condition expires")
        self.evaluate("""(() => {
            const node=document.createElement('div');
            node.id='zero-area-result';node.textContent='Clipped content';
            node.style.cssText='width:0;height:0;overflow:hidden;padding:0;border:0';
            document.querySelector('main').append(node);
            return true;
        })()""")
        zero_area = self.browser("wait", target_id=self.target, selector="#zero-area-result",
                                state="hidden", timeout_ms=500)
        self.check(zero_area["matched"] is True
                   and self.rejected("ssmcp.browser.wait", {"target_id": self.target,
                       "selector": "#zero-area-result", "state": "visible", "timeout_ms": 100}),
                   "wait treats an attached zero-area element as hidden and visible times out")
        self.evaluate("document.querySelector('#zero-area-result').remove();true")
        self.metrics["browser_wait"] = {"async_visible_elapsed_ms": waited["elapsed_ms"],
                                         "states_checked": ["visible", "hidden", "attached", "detached"],
                                         "zero_area_checked": True}

    def parallel_browser_checks(self, url):
        # Both targets are our local fixture in the ssmcp-owned temporary profile.
        # Page B must complete a native call while A's Promise is still gated;
        # no latency comparison or host-side network access substitutes for this.
        second = self.browser("open", url=url)["target_id"]
        gate = "ssmcp-e2e-parallel-" + str(time.monotonic_ns())
        expression_a = r"""(() => {
          const key = %s, channel = new BroadcastChannel(key);
          return new Promise(resolve => {
            const finish = released => {
              clearTimeout(timer); channel.close();
              localStorage.setItem(key, released ? 'released' : 'deadline');
              resolve({released, deadline_expired: !released});
            };
            const timer = setTimeout(() => finish(false), 5000);
            channel.onmessage = event => { if (event.data === 'release') finish(true); };
            localStorage.setItem(key, 'waiting');
          });
        })()""" % json.dumps(gate)
        expression_b = r"""(() => {
          const key = %s, deadline = performance.now() + 5000;
          return new Promise(resolve => {
            const observe = () => {
              const phase = localStorage.getItem(key);
              if (phase !== null) return resolve({observed_phase: phase});
              if (performance.now() >= deadline) return resolve({observed_phase: 'not-started'});
              setTimeout(observe, 10);
            };
            observe();
          });
        })()""" % json.dumps(gate)
        release = r"""(() => {
          const channel = new BroadcastChannel(%s);
          channel.postMessage('release'); channel.close(); return true;
        })()""" % json.dumps(gate)
        parameters = {"client": self.client_id, "a": self.target, "b": second,
                      "expression_a": expression_a, "expression_b": expression_b, "release": release}
        source = "const p=" + json.dumps(parameters) + ";" + r"""
          let aFinished = false;
          const request = (target_id, expression) => call(p.client, 'ssmcp.browser.evaluate', {target_id, expression});
          const value = r => { if (r.isError) throw Error(JSON.stringify(r)); return r.structuredContent.value; };
          const a = request(p.a, p.expression_a).then(
            r => { aFinished = true; return r; },
            e => { aFinished = true; return {isError:true,error:String(e)}; }
          );
          const b = (async () => {
            const observed = value(await request(p.b, p.expression_b));
            const completedBeforeA = !aFinished;
            const releaseRequested = value(await request(p.b, p.release));
            return {observed, completed_before_a:completedBeforeA, release_requested:releaseRequested};
          })();
          const results = await Promise.all([a,b]);
          return {page_a:value(results[0]), page_b:results[1]};
        """
        started = time.monotonic()
        try:
            observed = self.code(source)
            self.metrics["browser_parallel"] = {
                "scope": "two local fixture tabs in the ssmcp-owned temporary Chromium profile",
                "gate_deadline_ms": 5000, "elapsed_seconds": round(time.monotonic() - started, 3),
                **observed,
            }
            self.check(observed["page_b"]["observed"]["observed_phase"] == "waiting"
                       and observed["page_b"]["completed_before_a"],
                       "page B completes a native call while page A's Promise is still waiting")
            self.check(observed["page_b"]["release_requested"] is True
                       and observed["page_a"] == {"released": True, "deadline_expired": False},
                       "page A completes only after the explicit page B release, before its 5s deadline")
        finally:
            with contextlib.suppress(Exception):
                self.browser("evaluate", target_id=second,
                             expression="localStorage.removeItem(%s);true" % json.dumps(gate))
            self.browser("close", target_id=second)

    def desktop_checks(self):
        status = self.computer("status")
        self.metrics["desktop"] = status
        monitors = self.computer("monitors")["monitors"]
        self.check(bool(monitors) and all(m["scale"] > 0 for m in monitors), "desktop reports monitor logical geometry and scale")
        def own_window():
            return next((w for w in self.computer("windows")["windows"] if TITLE in w.get("title", "")), None)
        window = wait_for(own_window, timeout=10, description="owned fixture window")
        self.computer("focus", window_id=window["window_id"])
        self.evaluate("scrollTo(0,0);document.querySelector('#message').value='';true")
        geometry = self.evaluate("({w:innerWidth,h:innerHeight,ow:outerWidth,oh:outerHeight,dpr:devicePixelRatio})")
        # Window geometry and viewport are observed on the same focused test window.
        def point(selector, fraction=0.5):
            box = self.evaluate("(()=>{const r=document.querySelector(%s).getBoundingClientRect();return {x:r.x,y:r.y,w:r.width,h:r.height}})()" % json.dumps(selector))
            return {"x": round(window["x"] + (window["width"] - geometry["w"]) / 2 + box["x"] + box["w"] * fraction),
                    "y": round(window["y"] + window["height"] - geometry["h"] + box["y"] + box["h"] / 2)}
        region = {key: window[key] for key in ("x", "y", "width", "height")}
        mapping = self.screenshot("ssmcp.computer.screenshot", {"region": region, "max_width": 1100}, "desktop-before.jpg")
        self.check(mapping["coordinate_space"] == "desktop_logical" and mapping["image_to_desktop"]["scale_x"] > 0,
                   "desktop screenshot carries the image-to-logical coordinate mapping")
        self.computer("move", **point("#message"))
        self.computer("click", **point("#message"))
        text = "ssmcp 原生桌面 ✓"
        self.computer("type", text=text)
        self.check(self.evaluate("document.querySelector('#message').value") == text, "desktop Unicode typing reaches the focused test field")
        self.computer("key", key="a", modifiers=["ctrl"])
        self.computer("type", text=text + " verified")
        self.check(self.evaluate("document.querySelector('#message').value") == text + " verified",
                   "desktop modifier chord selects text before replacement")
        before = self.evaluate("probe.clicks")
        self.computer("click", **point("#run"))
        self.check(self.evaluate("probe.clicks") == before + 1, "desktop pointer click reaches the observed test button")
        start, end = point("#drag", 0.2), point("#drag", 0.8)
        self.computer("drag", path=[start, end], duration_ms=300)
        self.check(self.evaluate("probe.dragMoves>0 && probe.ups>=probe.downs"), "desktop drag moves while pressed and releases its button")
        self.computer("scroll", direction="down", steps=3, **point("#drag"))
        wait_for(lambda: self.evaluate("scrollY") > 0, description="desktop wheel scroll")
        self.check(True, "desktop wheel events scroll the test viewport")
        self.evaluate("scrollTo(0,0);true")
        self.screenshot("ssmcp.computer.screenshot", {"region": region, "max_width": 1100}, "desktop-after.jpg")
        self.check(self.rejected("ssmcp.computer.click", {"x": 1}), "desktop rejects incomplete coordinate pairs")

    def attached_browser_checks(self):
        port = free_port()
        profile = self.directory / "external-browser-profile"
        program = [self.chromium, "--headless=new", "--no-first-run", "--no-default-browser-check",
                   "--disable-background-networking", "--disable-dev-shm-usage", "--remote-debugging-address=127.0.0.1",
                   f"--remote-debugging-port={port}", f"--user-data-dir={profile}", "about:blank"]
        child = self.spawn("operator-browser-fixture", program)
        endpoint = f"http://127.0.0.1:{port}"
        wait_for(lambda: json_get(endpoint + "/json/version"), timeout=15, description="operator browser fixture")
        attached = self.browser("connect", endpoint=endpoint)
        self.check(attached["connected"] and not attached["owned"], "native connect attaches to an existing loopback browser")
        self.browser("disconnect")
        self.check(child.poll() is None and json_get(endpoint + "/json/version") is not None,
                   "disconnect preserves an attached operator-owned browser")
        self.stop(child)

    def run(self):
        self.prepare()
        workspace = self.directory / "workspace"
        workspace.mkdir()
        self.client, credentials = self.start_client(label="computer-use", name="computer-use-fixture", cwd=workspace)
        self.oauth_login(credentials)
        self.client_id = credentials["client_id"]
        tools = []
        cursor = None
        while True:
            args = {"client_id": self.client_id}
            if cursor:
                args["cursor"] = cursor
            page = self.public_call("list", args)
            tools.extend(page["tools"])
            cursor = page.get("nextCursor")
            if not cursor:
                break
        names = {tool["name"] for tool in tools}
        expected = {"ssmcp.browser." + name for name in ("status", "discover", "launch", "connect", "tabs", "open", "navigate", "snapshot", "click", "fill", "key", "scroll", "act", "wait", "wait_for", "evaluate", "screenshot", "close", "disconnect")}
        expected |= {"ssmcp.computer." + name for name in ("status", "monitors", "windows", "focus", "screenshot", "move", "click", "drag", "key", "type", "scroll")}
        expected |= {"ssmcp.omarchy." + name for name in ("status", "themes", "theme_set", "backgrounds", "background_set", "fonts", "font_set", "audio", "audio_set", "brightness", "brightness_set", "toggles", "toggle_set", "workspaces", "workspace_switch", "window_action", "apps", "app_launch", "menu", "battery", "network")}
        self.check(expected <= names, "all native computer, browser and Omarchy tools are discoverable through public list")
        discovery = self.browser("discover")
        self.check(discovery["discovery_only"] is True and discovery["selected_browser"] is None
                   and discovery["playwright_mcp"]["protocol_handshake"] == "not_attempted"
                   and not self.browser("status")["connected"],
                   "browser discovery reports observations without connecting or starting a browser")
        found = self.code("return await tools.search(%s,{query:'ssmcp.browser.',limit:50,include_schemas:true});"
                          % json.dumps(self.client_id))
        expected_browser = sorted((tool for tool in tools if tool["name"].startswith("ssmcp.browser.")),
                                  key=lambda tool: tool["name"])
        self.check(found["complete"] is True
                   and sorted(found["tools"], key=lambda tool: tool["name"]) == expected_browser,
                   "tools.search returns browser tools with descriptors identical to public list")
        self.metrics["browser_search"] = {"matches": len(found["tools"]), **found["stats"]}
        omarchy = self.call("ssmcp.omarchy.status", {})
        self.check(omarchy.get("platform") == "linux" and isinstance(omarchy.get("available"), bool),
                   "Omarchy status reports native platform availability without changing system settings")
        self.metrics["omarchy_available"] = omarchy["available"]
        self.check(not any(name.startswith(("claude.", "codex.", "pi.", "opencode.")) for name in names),
                   "control works without a coding-agent provider")
        self.browser_checks(self.start_fixture())
        self.attached_browser_checks()
        events, cursor = [], 0
        while True:
            page = self.call("ssmcp.audit.events", {"cursor": cursor, "max_events": 16})
            if page["truncated"]:
                raise AssertionError("control audit lost events during the bounded test")
            events.extend(page["events"])
            cursor = page["next_cursor"]
            if not page["events"]:
                break
        self.check(True, "control audit retains the complete requested event range")
        completed = [entry["event"] for entry in events if entry["event"].get("type") == "ssmcp.control.completed"]
        self.check(any(event["provider"] == "browser" and event["success"] for event in completed), "native control calls produce completion audit events")
        self.check(any(event["provider"] == "omarchy" and event["success"] for event in completed),
                   "native Omarchy observations produce completion audit events")
        self.metrics["native_tools"] = len(expected)
        self.metrics["desktop_exercised"] = self.desktop

    def cleanup(self):
        if self.http_fixture:
            self.http_fixture.shutdown()
            self.http_fixture.server_close()
        super().cleanup()

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, default=Path("target/debug"))
    parser.add_argument("--evidence-dir", type=Path, default=Path("target/computer-use-evidence"))
    parser.add_argument("--report", type=Path)
    parser.add_argument("--desktop", action="store_true")
    parser.add_argument("--chromium")
    args = parser.parse_args()
    candidates = [args.chromium, "/usr/lib/chromium/chromium", "/opt/google/chrome/chrome", shutil.which("chromium"), shutil.which("google-chrome")]
    chromium = next((p for p in candidates if p and Path(p).is_file()), None)
    if not chromium:
        parser.error("install a system Chromium browser or pass --chromium")
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-computer-use-") as directory:
        harness = ComputerUse(Path(directory), args.bin_dir, args.evidence_dir, args.desktop, chromium)
        try:
            harness.run()
        except Exception:
            harness.diagnostics()
            raise
        finally:
            harness.cleanup()
        report = {"status": "passed", "checks": harness.checks, "metrics": harness.metrics,
                  "duration_seconds": round(time.monotonic() - started, 3),
                  "transport": "OpenSSH public-key + OAuth PKCE + HTTP MCP + native Chromium CDP"}
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
        print(f"Passed {len(harness.checks)} native control checks in {report['duration_seconds']}s")
if __name__ == "__main__":
    main()
