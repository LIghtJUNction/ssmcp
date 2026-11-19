#!/usr/bin/env python3
"""Local guide routes and real 120x30 sidebar QR, using an owned Xvfb/ztty."""
import argparse, base64, contextlib, json, os, shutil, signal, subprocess, tempfile, time
import urllib.request
from pathlib import Path
from urllib.parse import urlsplit
import e2e
from e2e_native_images import Window, binary_identity, export_logs
from e2e_input_history import InputHistory
from e2e_dashboard import sha256
class Guide(InputHistory):
    def config(self, name, **options):
        path = super().config(name, **options)
        path.write_text(path.read_text().replace("sidebar=false", "sidebar=true\nsidebar_width=37"))
        return path


def browser_screenshot(chrome, profile, url, destination, width, height, mobile):
    """Measure the CSS viewport on our loopback page, then save that exact view."""
    import websocket
    profile.mkdir()
    process = connection = None
    with (profile / "stderr").open("wb") as log:
        try:
            process = subprocess.Popen([
                chrome, "--headless=new", "--no-sandbox", "--disable-gpu",
                "--disable-dev-shm-usage", "--hide-scrollbars", "--no-first-run", "--no-default-browser-check",
                "--remote-debugging-address=127.0.0.1", "--remote-debugging-port=0",
                "--user-data-dir=" + str(profile), "about:blank",
            ], stdout=subprocess.DEVNULL, stderr=log, start_new_session=True)
            active = profile / "DevToolsActivePort"
            deadline = time.monotonic() + 10
            while not active.exists():
                if process.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError("owned Chromium did not expose its private debugger")
                time.sleep(0.025)
            port = int(active.read_text().splitlines()[0])
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with opener.open(f"http://127.0.0.1:{port}/json/list", timeout=5) as response:
                pages = json.load(response)
            endpoint = next(page["webSocketDebuggerUrl"] for page in pages if page["type"] == "page")
            parsed = urlsplit(endpoint)
            assert parsed.hostname in ("127.0.0.1", "localhost") and parsed.port == port
            connection = websocket.create_connection(endpoint, timeout=5, suppress_origin=True)
            sequence = 0

            def call(method, params=None):
                nonlocal sequence
                sequence += 1
                connection.send(json.dumps({"id": sequence, "method": method, "params": params or {}}))
                while True:
                    message = json.loads(connection.recv())
                    if message.get("id") == sequence:
                        if "error" in message:
                            raise RuntimeError("private browser command failed: " + method)
                        return message["result"]

            call("Page.enable")
            call("Emulation.setDeviceMetricsOverride", {
                "width": width, "height": height, "deviceScaleFactor": 1, "mobile": mobile,
            })
            call("Page.navigate", {"url": url})
            deadline = time.monotonic() + 10
            while True:
                result = call("Runtime.evaluate", {"expression":
                    "({url:location.href,state:document.readyState,width:innerWidth,height:innerHeight,"
                    "layout:document.documentElement.clientWidth,scroll:document.documentElement.scrollWidth,"
                    "title:document.title})", "returnByValue": True})
                metrics = result.get("result", {}).get("value", {})
                if metrics.get("url") == url and metrics.get("state") == "complete":
                    break
                if time.monotonic() > deadline:
                    raise RuntimeError("candidate guide did not finish rendering")
                time.sleep(0.025)
            assert metrics["width"] == width and metrics["height"] == height, metrics
            assert metrics["layout"] == width and metrics["scroll"] <= width, metrics
            screenshot = call("Page.captureScreenshot", {"format": "png", "captureBeyondViewport": False})
            destination.write_bytes(base64.b64decode(screenshot["data"], validate=True))
            from PIL import Image
            with Image.open(destination) as image:
                assert image.size == (width, height), image.size
            return metrics
        finally:
            if connection is not None:
                with contextlib.suppress(Exception):
                    connection.close()
            if process is not None and process.poll() is None:
                with contextlib.suppress(ProcessLookupError):
                    os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    with contextlib.suppress(ProcessLookupError):
                        os.killpg(process.pid, signal.SIGKILL)
                    process.wait(timeout=3)

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bin-dir", type=Path, required=True)
    parser.add_argument("--ztty", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    binary_dir, ztty, output = args.bin_dir.resolve(), args.ztty.resolve(), args.out.resolve()
    decoder = binary_dir / "vt_snapshot"
    if not decoder.is_file():
        decoder = binary_dir / "examples/vt_snapshot"
    for path in (binary_dir / "ssmcp", binary_dir / "ssmcpd", decoder, ztty):
        if not path.is_file():
            parser.error("missing already-built binary: " + str(path))
    output.mkdir(parents=True, exist_ok=True)
    report = {"status": "failed", "binaries": {p.name: sha256(p) for p in (binary_dir / "ssmcp", binary_dir / "ssmcpd", ztty)}}
    report["terminal"] = binary_identity(ztty)
    start = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="ssmcp-guide-acceptance-") as directory:
        temp = Path(directory)
        h = Guide(temp, binary_dir, decoder, output)
        window = None
        ready = False
        try:
            # This gateway advertises a reserved HTTPS origin. Its real guide
            # pages are tested via loopback below; the live QR must select the
            # exact same-origin portal fallback when discovery is unavailable.
            h.prepare()
            routes = []
            for path, title, language in [
                ("/guide/", "Guide · ssmcp", "en"),
                ("/guide/en", "Guide · ssmcp", "en"),
                ("/guide/zh", "使用指南 · ssmcp", "zh-CN"),
                ("/guide/style.css", None, None),
            ]:
                status, body, headers = h.request("GET", path=path, secret="", headers={"Accept": "text/html, text/css"})
                lower = {k.lower(): v for k, v in headers.items()}
                content_type = lower.get("content-type", "")
                h.check(status == 200, "unauthenticated candidate GET " + path + " returns 200")
                if title:
                    h.check("text/html" in content_type and "<title>" + title + "</title>" in body
                            and '<html lang="' + language + '">' in body, path + " returns the correct localized guide")
                    h.check("{{" not in body and h.issuer + "/mcp" in body
                            and "ssmcp.fs.read_image" in body, path + " renders real gateway examples")
                    file = "guide-zh.html" if language == "zh-CN" else "guide-en.html"
                else:
                    h.check("text/css" in content_type and body.strip(), "shared guide CSS is served as CSS")
                    file = "style.css"
                (output / file).write_text(body)
                routes.append({"path": path, "status": status, "content_type": content_type, "title": title})
            report["routes"] = routes
            chrome = shutil.which("chromium") or shutil.which("chromium-browser")
            if not chrome:
                raise RuntimeError("Chromium is required for actual route screenshots")
            report["browser"] = binary_identity(Path(chrome).resolve())
            report["viewports"] = {}
            for label, route, width, height, mobile in [
                ("english-desktop", "/guide/", 1365, 900, False),
                ("chinese-mobile", "/guide/zh", 390, 844, True),
            ]:
                report["viewports"][label] = browser_screenshot(
                    chrome, temp / ("chrome-" + label), f"http://127.0.0.1:{h.http_port}" + route,
                    output / (label + ".png"), width, height, mobile,
                )
                h.check(True, label + " renders at the measured CSS viewport without horizontal overflow")
            h.workspace = temp / "workspace"
            h.workspace.mkdir()
            subprocess.run(["git", "-c", "init.defaultBranch=main", "init", "-q"], cwd=h.workspace,
                env={**os.environ, **h.safe_git_env, "HOME": str(h.home)}, check=True, timeout=5,
                stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            window = Window(h, ztty, temp)
            credentials = window.connection()
            h.client_id = credentials["client_id"]
            h.validate_connection_record(credentials)
            h.oauth_login(credentials)
            window.wait(lambda: h.ui("/display/rows") == 32, "candidate UI")
            ready = True
            window.resize(30, 120)
            window.wait(lambda: h.ui("/display/rows") == 30 and h.ui("/display/cols") == 120, "120x30 terminal")
            endpoint = urlsplit(credentials["url"])
            assert endpoint.username is None and endpoint.password is None
            expected = endpoint.scheme + "://" + endpoint.netloc + "/"
            h.check(expected == h.issuer + "/", "the owned HTTPS fixture has the expected public origin")
            # Wait beyond the two-second anonymous discovery budget so an
            # initial portal placeholder cannot count as settled fallback.
            discovery_settled = time.monotonic() + 2.1
            zbar = shutil.which("zbarimg")
            if not zbar:
                raise RuntimeError("zbarimg is required for a real QR decode")
            shot = output / "sidebar-120x30.png"
            last_decoded = None
            def decoded():
                nonlocal last_decoded
                window.image().save(shot)
                r = subprocess.run([zbar, "--quiet", "--raw", str(shot)], capture_output=True, text=True, timeout=4)
                last_decoded = r.stdout.strip() if r.returncode == 0 else None
                return last_decoded if last_decoded == expected and time.monotonic() >= discovery_settled else None
            try:
                value = window.wait(decoded, "QR decoder reads the settled same-origin portal fallback", 10)
            except AssertionError as error:
                raise AssertionError(f"{error}; expected={expected!r}; last decoded={last_decoded!r}") from error
            h.check(value == expected, "actual 120x30 sidebar QR decodes exactly to the credential-free same-origin portal fallback")
            (output / "qr-decoded.txt").write_text(value + "\n")
            report["qr_url"] = value
            report["qr_route"] = {"selection": "portal_fallback", "reason": "advertised reserved HTTPS origin is unavailable",
                                  "expected_url": expected, "discovery_budget_seconds": 2}
            report["cell_pixels"] = [window.cw, window.ch]
            window.resize(16, 60)
            window.wait(lambda: h.ui("/display/rows") == 16 and h.ui("/display/cols") == 60, "small terminal")
            time.sleep(0.15)
            narrow = output / "sidebar-60x16.png"
            window.image().save(narrow)
            r = subprocess.run([zbar, "--quiet", "--raw", str(narrow)], capture_output=True, text=True, timeout=4)
            h.check(r.returncode != 0 and not r.stdout.strip(), "a small terminal switches away from a clipped QR")
            h.check(h.ui("/display/conversation/draft/text") == "", "graphics replies and resizing leave the guide session draft empty")
            report["status"] = "passed"
        except Exception as error:
            detail = type(error).__name__ + ": " + str(error)
            for secret in [h.secret, *h.sensitive]:
                if secret: detail = detail.replace(secret, "[redacted]")
            report["error"] = detail
            if window and ready:
                with contextlib.suppress(Exception):
                    window.image().save(output / "failure.png")
            print(detail, flush=True)
        finally:
            if window: window.close()
            h.close_frontends()
            h.cleanup()
            export_logs(h, temp, output)
            report.update(checks=h.checks, metrics=h.metrics, duration_seconds=round(time.monotonic()-start,3))
            (output / "report.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(json.dumps({"status": report["status"], "checks": len(report["checks"]), "report": str(output / "report.json")}), flush=True)
    return 0 if report["status"] == "passed" else 1

if __name__ == "__main__":
    def interrupted(*_): raise TimeoutError("guide validation interrupted")
    signal.signal(signal.SIGTERM, interrupted)
    raise SystemExit(main())

